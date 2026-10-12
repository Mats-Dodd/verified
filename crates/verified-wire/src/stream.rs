//! Durable Streams: offsets, cursors, control events and the names both ends
//! agree on.
//!
//! Follows q's `server/src/http/SessionStreamHttp.ts`, `domain/Offset.ts` and
//! `domain/Cursor.ts` (Durable Streams PROTOCOL.md). A session stream is
//! append-only; an [`Offset`] is the seq of the next chunk to read, so reading
//! from an offset returns every chunk with `seq >= offset`.
//!
//! - Catch-up: `GET /sessions/{id}/stream?offset=<o>` answers the chunks from
//!   `o` as a JSON array with [`header::STREAM_NEXT_OFFSET`] and
//!   [`header::STREAM_UP_TO_DATE`]. A catch-up of a range never changes, so it
//!   carries [`catch_up_etag`] and [`CATCH_UP_CACHE_CONTROL`]; `offset=now`
//!   (the end) is [`NO_STORE`].
//! - Live: `...&live=sse&cursor=<c>` streams SSE. Every read is a
//!   [`EVENT_DATA`] event with its chunks (omitted when empty) followed by an
//!   [`EVENT_CONTROL`] event ([`Control`]). The server closes the connection
//!   after a lifetime; the client reconnects from the last
//!   `streamNextOffset`, echoing the last `streamCursor`.

use core::fmt;
use core::str::FromStr;

use serde::de::{self, DeserializeSeed, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::WireLimits;
use crate::chunk::UiChunk;
use crate::sse::{SseError, SseEvent};
use verified_domain::id::{SessionId, UnixMillis};

/// HTTP header names.
pub mod header {
    /// The offset to read from next.
    pub const STREAM_NEXT_OFFSET: &str = "stream-next-offset";
    /// `true` when the response reached the end of the stream.
    pub const STREAM_UP_TO_DATE: &str = "stream-up-to-date";
    /// Marks a response body as an AI SDK UI message stream.
    pub const UI_MESSAGE_STREAM: &str = "x-vercel-ai-ui-message-stream";
    /// The UI message stream protocol version.
    pub const UI_MESSAGE_STREAM_VERSION: &str = "v1";
}

/// Query parameter names and values of a stream read.
pub mod query {
    /// Where to read from: an [`Offset`](super::Offset) or `now`.
    pub const OFFSET: &str = "offset";
    /// Present to follow the stream live.
    pub const LIVE: &str = "live";
    /// The only live mode: server-sent events.
    pub const LIVE_SSE: &str = "sse";
    /// The cursor echoed from the last control event.
    pub const CURSOR: &str = "cursor";
}

/// SSE event carrying a JSON array of chunks.
pub const EVENT_DATA: &str = "data";
/// SSE event carrying a [`Control`].
pub const EVENT_CONTROL: &str = "control";
/// `cache-control` of a catch-up from an offset: the range never changes.
pub const CATCH_UP_CACHE_CONTROL: &str = "private, max-age=60, stale-while-revalidate=300";
/// `cache-control` of a read from `now` and of live responses.
pub const NO_STORE: &str = "no-store";

/// The `etag` of a catch-up: the same range of an append-only stream is
/// always the same chunks.
#[must_use]
pub fn catch_up_etag(session: SessionId, from: Offset, next: Offset) -> String {
    format!("\"{session}:{from}:{next}\"")
}

/// Why a stream value did not parse.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("offset is not `-1` or 16 digits")]
    BadOffset,
    #[error("seq {seq} is past the largest offset")]
    OffsetOutOfRange { seq: u64 },
    #[error("cursor is not a decimal integer")]
    BadCursor,
    #[error("unknown stream event `{name}`")]
    UnknownEvent { name: String },
    #[error("a read carries more than {limit} chunks")]
    TooManyChunks { limit: usize },
    #[error("malformed stream JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sse(#[from] SseError),
}

/// A position in a session stream: the seq of the next chunk to read.
///
/// On the wire, the seq zero-padded to 16 digits, so sorting as text sorts as
/// numbers; the protocol's `-1` ("from the beginning") parses to
/// [`Offset::BEGINNING`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Offset(u64);

impl Offset {
    /// The beginning of every stream.
    pub const BEGINNING: Self = Self(0);
    /// The largest seq 16 digits hold.
    pub const MAX_SEQ: u64 = 9_999_999_999_999_999;
    const WIDTH: usize = 16;

    /// The offset of the chunk at `seq`.
    ///
    /// # Errors
    /// [`StreamError::OffsetOutOfRange`] past [`Offset::MAX_SEQ`].
    pub const fn at(seq: u64) -> Result<Self, StreamError> {
        if seq > Self::MAX_SEQ {
            Err(StreamError::OffsetOutOfRange { seq })
        } else {
            Ok(Self(seq))
        }
    }

    /// The offset just past the chunk at `seq`.
    ///
    /// # Errors
    /// [`StreamError::OffsetOutOfRange`] past [`Offset::MAX_SEQ`].
    pub const fn after(seq: u64) -> Result<Self, StreamError> {
        match seq.checked_add(1) {
            Some(next) => Self::at(next),
            None => Err(StreamError::OffsetOutOfRange { seq }),
        }
    }

    /// The seq of the next chunk to read.
    #[must_use]
    pub const fn seq(self) -> u64 {
        self.0
    }

    /// # Errors
    /// [`StreamError::BadOffset`] for anything but `-1` or 16 ASCII digits.
    pub fn parse(text: &str) -> Result<Self, StreamError> {
        if text == "-1" {
            return Ok(Self::BEGINNING);
        }
        if text.len() != Self::WIDTH || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(StreamError::BadOffset);
        }
        text.parse()
            .map(Self)
            .map_err(|_digits| StreamError::BadOffset)
    }
}

impl fmt::Display for Offset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016}", self.0)
    }
}

impl FromStr for Offset {
    type Err = StreamError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for Offset {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Offset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OffsetVisitor;

        impl Visitor<'_> for OffsetVisitor {
            type Value = Offset;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a stream offset")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Offset, E> {
                Offset::parse(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(OffsetVisitor)
    }
}

/// Where a read starts: an offset, or `now` for only what is appended from here on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFrom {
    Now,
    Offset(Offset),
}

impl ReadFrom {
    /// # Errors
    /// [`StreamError::BadOffset`] for anything but `now` or an offset.
    pub fn parse(text: &str) -> Result<Self, StreamError> {
        if text == "now" {
            Ok(Self::Now)
        } else {
            Offset::parse(text).map(Self::Offset)
        }
    }
}

impl fmt::Display for ReadFrom {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Now => formatter.write_str("now"),
            Self::Offset(offset) => fmt::Display::fmt(offset, formatter),
        }
    }
}

/// Jitter for [`Cursor::next`]: 1 to [`Jitter::MAX_INTERVALS`] intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jitter(u64);

impl Jitter {
    /// An hour of 20 second intervals.
    pub const MAX_INTERVALS: u64 = 180;

    /// Jitter drawn from Entropy seam bytes.
    #[must_use]
    pub const fn from_entropy(entropy: u64) -> Self {
        match entropy.checked_rem(Self::MAX_INTERVALS) {
            Some(remainder) => Self(remainder.saturating_add(1)),
            None => Self(1),
        }
    }

    #[must_use]
    pub const fn intervals(self) -> u64 {
        self.0
    }
}

/// A Durable Streams cursor (PROTOCOL.md §10.1): live responses carry one
/// that changes over time, so CDNs collapsing live requests on `cursor` never
/// serve the same cached response forever.
///
/// The number of 20 second intervals since 2024-10-09T00:00:00Z. Cursors
/// never go backwards: when the client echoes one at or ahead of the current
/// interval, the next is that plus some jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor(u64);

impl Cursor {
    const EPOCH_MILLIS: u64 = 1_728_432_000_000;
    const INTERVAL_MILLIS: u64 = 20_000;

    /// The cursor for a live response at `now` (Clock seam data), given the
    /// cursor the client echoed and jitter from the Entropy seam.
    #[must_use]
    pub const fn next(now: UnixMillis, echoed: Option<Self>, jitter: Jitter) -> Self {
        let elapsed = now.get().saturating_sub(Self::EPOCH_MILLIS);
        let current = match elapsed.checked_div(Self::INTERVAL_MILLIS) {
            Some(intervals) => intervals,
            None => 0,
        };
        match echoed {
            Some(Self(client)) if client >= current => {
                Self(client.saturating_add(jitter.intervals()))
            }
            Some(_) | None => Self(current),
        }
    }

    /// # Errors
    /// [`StreamError::BadCursor`] for anything but a decimal integer.
    pub fn parse(text: &str) -> Result<Self, StreamError> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(StreamError::BadCursor);
        }
        text.parse()
            .map(Self)
            .map_err(|_digits| StreamError::BadCursor)
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl FromStr for Cursor {
    type Err = StreamError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CursorVisitor;

        impl Visitor<'_> for CursorVisitor {
            type Value = Cursor;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a stream cursor")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Cursor, E> {
                Cursor::parse(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(CursorVisitor)
    }
}

/// Whether a read reached the end of the stream (`upToDate` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "bool", into = "bool")]
pub enum Freshness {
    UpToDate,
    Behind,
}

impl From<bool> for Freshness {
    fn from(up_to_date: bool) -> Self {
        if up_to_date {
            Self::UpToDate
        } else {
            Self::Behind
        }
    }
}

impl From<Freshness> for bool {
    fn from(freshness: Freshness) -> Self {
        match freshness {
            Freshness::UpToDate => true,
            Freshness::Behind => false,
        }
    }
}

/// The control event that follows every live read (§5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Control {
    /// Where to reconnect from.
    pub stream_next_offset: Offset,
    /// To echo as the `cursor` query parameter on reconnect.
    pub stream_cursor: Cursor,
    pub up_to_date: Freshness,
}

/// One event of a live session stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Chunks, in stream order.
    Data(Vec<UiChunk>),
    Control(Control),
}

impl StreamEvent {
    /// The SSE event the server sends.
    ///
    /// # Panics
    /// Never: chunks and control serialize infallibly to JSON without raw
    /// carriage returns.
    #[must_use]
    pub fn to_sse(&self) -> SseEvent {
        let (name, json) = match self {
            Self::Data(chunks) => (EVENT_DATA, serde_json::to_string(chunks)),
            Self::Control(control) => (EVENT_CONTROL, serde_json::to_string(control)),
        };
        let json = json.expect("chunks and control have string keys and serialize infallibly");
        SseEvent::new(name.to_owned(), json)
            .expect("event names are fixed tokens and JSON escapes carriage returns")
    }

    /// The stream event an SSE event carries.
    ///
    /// # Errors
    /// An unknown event name, malformed JSON, or more chunks than
    /// `limits.max_chunks_per_read`.
    pub fn from_sse(event: &SseEvent, limits: &WireLimits) -> Result<Self, StreamError> {
        match event.name() {
            EVENT_DATA => decode_chunks(event.payload().as_bytes(), limits).map(Self::Data),
            EVENT_CONTROL => Ok(Self::Control(serde_json::from_str(event.payload())?)),
            name => Err(StreamError::UnknownEvent {
                name: name.to_owned(),
            }),
        }
    }
}

/// A JSON array of chunks (an SSE `data` event or a catch-up body), at most
/// `limits.max_chunks_per_read` long; checked while decoding.
///
/// # Errors
/// Malformed JSON, a malformed known chunk, or too many chunks.
pub fn decode_chunks(json: &[u8], limits: &WireLimits) -> Result<Vec<UiChunk>, StreamError> {
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    let chunks = BoundedChunks {
        limit: limits.max_chunks_per_read,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    chunks.ok_or(StreamError::TooManyChunks {
        limit: limits.max_chunks_per_read,
    })
}

/// Encodes chunks as a JSON array (a catch-up body).
///
/// # Panics
/// Never: chunks serialize infallibly.
#[must_use]
pub fn encode_chunks(chunks: &[UiChunk]) -> Vec<u8> {
    serde_json::to_vec(chunks).expect("chunks have string keys and serialize infallibly")
}

/// Decodes at most `limit` chunks; `None` when there are more (the rest are
/// skipped without being kept).
struct BoundedChunks {
    limit: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedChunks {
    type Value = Option<Vec<UiChunk>>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for BoundedChunks {
    type Value = Option<Vec<UiChunk>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of chunks")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut chunks = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.limit));
        while chunks.len() < self.limit {
            match seq.next_element()? {
                Some(chunk) => chunks.push(chunk),
                None => return Ok(Some(chunks)),
            }
        }
        if seq.next_element::<IgnoredAny>()?.is_none() {
            return Ok(Some(chunks));
        }
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn offsets_are_sixteen_digits_and_minus_one_is_the_beginning() {
        assert_eq!(Offset::parse("-1").ok(), Some(Offset::BEGINNING));
        let offset = Offset::parse("0000000000000042").expect("an offset");
        assert_eq!(offset.seq(), 42);
        assert_eq!(offset.to_string(), "0000000000000042");
        assert_eq!(Offset::after(41).ok(), Some(offset));
        for bad in ["42", "+000000000000042", "00000000000000042", "now", ""] {
            assert!(Offset::parse(bad).is_err(), "{bad}");
        }
        assert!(matches!(
            Offset::after(Offset::MAX_SEQ),
            Err(StreamError::OffsetOutOfRange { .. })
        ));
        assert_eq!(ReadFrom::parse("now").ok(), Some(ReadFrom::Now));
    }

    #[test]
    fn cursors_never_go_backwards() {
        let now = UnixMillis::new(1_728_432_000_000 + 20_000 * 10 + 5).expect("in range");
        let jitter = Jitter::from_entropy(6);
        assert_eq!(Cursor::next(now, None, jitter), Cursor(10));
        assert_eq!(Cursor::next(now, Some(Cursor(3)), jitter), Cursor(10));
        assert_eq!(Cursor::next(now, Some(Cursor(10)), jitter), Cursor(17));
        assert_eq!(
            Cursor::next(UnixMillis::new(0).expect("in range"), None, jitter),
            Cursor(0)
        );
    }

    #[test]
    fn control_events_use_protocol_names() {
        let control = StreamEvent::Control(Control {
            stream_next_offset: Offset::after(2).expect("in range"),
            stream_cursor: Cursor(9),
            up_to_date: Freshness::UpToDate,
        });
        let sse = control.to_sse();
        assert_eq!(sse.name(), "control");
        assert_eq!(
            sse.payload(),
            r#"{"streamNextOffset":"0000000000000003","streamCursor":"9","upToDate":true}"#
        );
        assert_eq!(
            StreamEvent::from_sse(&sse, &WireLimits::DEFAULT).ok(),
            Some(control)
        );
    }

    #[test]
    fn data_events_are_bounded_and_unknown_events_rejected() {
        let limits = WireLimits {
            max_chunks_per_read: 2,
            ..WireLimits::DEFAULT
        };
        let two = StreamEvent::Data(vec![UiChunk::StartStep, UiChunk::FinishStep]);
        assert_eq!(
            StreamEvent::from_sse(&two.to_sse(), &limits).ok(),
            Some(two)
        );
        let three = StreamEvent::Data(vec![UiChunk::StartStep; 3]);
        assert!(matches!(
            StreamEvent::from_sse(&three.to_sse(), &limits),
            Err(StreamError::TooManyChunks { limit: 2 })
        ));
        let other = SseEvent::new("other".to_owned(), "[]".to_owned()).expect("an event");
        assert!(matches!(
            StreamEvent::from_sse(&other, &limits),
            Err(StreamError::UnknownEvent { .. })
        ));
        assert!(matches!(
            decode_chunks(b"[] x", &limits),
            Err(StreamError::Json(_))
        ));
    }

    #[test]
    fn etags_name_the_range() {
        let session = SessionId::parse("01a10400-0000-7000-8000-000000000001").expect("an id");
        assert_eq!(
            catch_up_etag(
                session,
                Offset::BEGINNING,
                Offset::after(4).expect("in range")
            ),
            "\"01a10400-0000-7000-8000-000000000001:0000000000000000:0000000000000005\""
        );
    }

    proptest! {
        #[test]
        fn offsets_round_trip_and_sort_as_text(one in 0..=Offset::MAX_SEQ, other in 0..=Offset::MAX_SEQ) {
            let (first, second) = (Offset::at(one).expect("in range"), Offset::at(other).expect("in range"));
            prop_assert_eq!(Offset::parse(&first.to_string()).ok(), Some(first));
            prop_assert_eq!(first.cmp(&second), first.to_string().cmp(&second.to_string()));
        }
    }
}
