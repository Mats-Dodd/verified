//! Durable Streams: offsets, control events and the names both ends agree
//! on.
//!
//! Follows q's `server/src/http/SessionStreamHttp.ts` and `domain/Offset.ts`
//! (Durable Streams PROTOCOL.md), cut to what the TUI uses. A session stream
//! is append-only; an [`Offset`] is the seq of the next chunk to read, so
//! reading from an offset returns every chunk with `seq >= offset`.
//!
//! `GET /actors/{id}/stream?offset=<o>` streams SSE. Every read is a
//! [`EVENT_DATA`] event with its chunks (omitted when empty) followed by an
//! [`EVENT_CONTROL`] event ([`Control`]). The server closes the connection
//! after a lifetime; the client reconnects from the last `streamNextOffset`.

use core::fmt;
use core::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::chunk::UiChunk;
use crate::sse::{SseError, SseEvent};

/// HTTP header names.
pub mod header {
    /// Marks a response body as an AI SDK UI message stream.
    pub const UI_MESSAGE_STREAM: &str = "x-vercel-ai-ui-message-stream";
    /// The UI message stream protocol version.
    pub const UI_MESSAGE_STREAM_VERSION: &str = "v1";
}

/// Query parameter names of a stream read.
pub mod query {
    /// Where to read from: an [`Offset`](super::Offset).
    pub const OFFSET: &str = "offset";
}

/// SSE event carrying a JSON array of chunks.
pub const EVENT_DATA: &str = "data";
/// SSE event carrying a [`Control`].
pub const EVENT_CONTROL: &str = "control";
/// Why a stream value did not parse.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("offset is not 16 digits")]
    BadOffset,
    #[error("seq {seq} is past the largest offset")]
    OffsetOutOfRange { seq: u64 },
    #[error("unknown stream event `{name}`")]
    UnknownEvent { name: String },
    #[error("malformed stream JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sse(#[from] SseError),
}

/// A position in a session stream: the seq of the next chunk to read.
///
/// On the wire, the seq zero-padded to 16 digits, so sorting as text sorts as
/// numbers.
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
    /// [`StreamError::BadOffset`] for anything but 16 ASCII digits.
    pub fn parse(text: &str) -> Result<Self, StreamError> {
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

    /// The stream event an SSE event carries; the decoder already bounded
    /// its size.
    ///
    /// # Errors
    /// An unknown event name, or malformed JSON.
    pub fn from_sse(event: &SseEvent) -> Result<Self, StreamError> {
        match event.name() {
            EVENT_DATA => decode_chunks(event.payload().as_bytes()).map(Self::Data),
            EVENT_CONTROL => Ok(Self::Control(serde_json::from_str(event.payload())?)),
            name => Err(StreamError::UnknownEvent {
                name: name.to_owned(),
            }),
        }
    }
}

/// A JSON array of chunks (an SSE `data` event).
///
/// # Errors
/// Malformed JSON, or a malformed known chunk.
pub fn decode_chunks(json: &[u8]) -> Result<Vec<UiChunk>, StreamError> {
    Ok(serde_json::from_slice(json)?)
}

/// Encodes chunks as a JSON array (an SSE `data` event's payload).
///
/// # Panics
/// Never: chunks serialize infallibly.
#[must_use]
pub fn encode_chunks(chunks: &[UiChunk]) -> Vec<u8> {
    serde_json::to_vec(chunks).expect("chunks have string keys and serialize infallibly")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn offsets_are_sixteen_digits() {
        let offset = Offset::parse("0000000000000042").expect("an offset");
        assert_eq!(offset.seq(), 42);
        assert_eq!(offset.to_string(), "0000000000000042");
        assert_eq!(Offset::after(41).ok(), Some(offset));
        for bad in [
            "42",
            "+000000000000042",
            "00000000000000042",
            "-1",
            "now",
            "",
        ] {
            assert!(Offset::parse(bad).is_err(), "{bad}");
        }
        assert!(matches!(
            Offset::after(Offset::MAX_SEQ),
            Err(StreamError::OffsetOutOfRange { .. })
        ));
    }

    #[test]
    fn control_events_use_protocol_names() {
        let control = StreamEvent::Control(Control {
            stream_next_offset: Offset::after(2).expect("in range"),
            up_to_date: Freshness::UpToDate,
        });
        let sse = control.to_sse();
        assert_eq!(sse.name(), "control");
        assert_eq!(
            sse.payload(),
            r#"{"streamNextOffset":"0000000000000003","upToDate":true}"#
        );
        assert_eq!(StreamEvent::from_sse(&sse).ok(), Some(control));
    }

    #[test]
    fn data_events_round_trip_and_unknown_events_are_rejected() {
        let two = StreamEvent::Data(vec![UiChunk::StartStep, UiChunk::FinishStep]);
        assert_eq!(StreamEvent::from_sse(&two.to_sse()).ok(), Some(two));
        let other = SseEvent::new("other".to_owned(), "[]".to_owned()).expect("an event");
        assert!(matches!(
            StreamEvent::from_sse(&other),
            Err(StreamError::UnknownEvent { .. })
        ));
        assert!(matches!(decode_chunks(b"[] x"), Err(StreamError::Json(_))));
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
