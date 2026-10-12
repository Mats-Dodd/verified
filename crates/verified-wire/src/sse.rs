//! Server-sent events framing: encode and incremental decode.
//!
//! Just enough of the event-stream format
//! (<https://html.spec.whatwg.org/multipage/server-sent-events.html>) for
//! session streams: `event` and `data` fields, comments, and every line
//! terminator the format allows (CRLF, LF, CR). `id` and `retry` are ignored;
//! Durable Streams carries offsets in control events instead.
//!
//! [`SseDecoder`] is a pure push parser (bytes in, events out) with a bounded
//! line buffer and a bounded event buffer, so it gives the same events however
//! the bytes are split. Ported from q's `src/sse.rs`, which read a blocking
//! `BufRead`.

use crate::WireLimits;

/// Why bytes are not an event stream within the limits, or an event cannot
/// be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("SSE line longer than {limit} bytes")]
    LineTooLong { limit: usize },
    #[error("SSE event data longer than {limit} bytes")]
    EventTooLarge { limit: usize },
    #[error("SSE line is not UTF-8")]
    NotUtf8,
    #[error("SSE event name is empty or contains a line break")]
    BadName,
    #[error("SSE event data contains a carriage return")]
    BadData,
}

/// The event name a stream uses when an event has no `event:` field.
pub const DEFAULT_EVENT: &str = "message";

/// A comment line servers send to keep idle connections open.
pub const KEEP_ALIVE: &str = ":\n\n";

/// One dispatched event: a name (never empty, no line breaks) and data
/// (no carriage returns; line feeds become separate `data:` lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    name: String,
    payload: String,
}

impl SseEvent {
    /// # Errors
    /// [`SseError::BadName`] for an empty name or one with a line break;
    /// [`SseError::BadData`] for data with a carriage return (which the
    /// format would read back as a line break).
    pub fn new(name: String, payload: String) -> Result<Self, SseError> {
        if name.is_empty() || name.contains(['\n', '\r']) {
            return Err(SseError::BadName);
        }
        if payload.contains('\r') {
            return Err(SseError::BadData);
        }
        Ok(Self { name, payload })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }

    #[must_use]
    pub fn into_payload(self) -> String {
        self.payload
    }

    /// Appends the event's wire form to `out`.
    pub fn encode_into(&self, out: &mut String) {
        out.push_str("event: ");
        out.push_str(&self.name);
        out.push('\n');
        for line in self.payload.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }

    /// The event's wire form.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(
            self.name
                .len()
                .saturating_add(self.payload.len())
                .saturating_add(32),
        );
        self.encode_into(&mut out);
        out
    }
}

/// Whether the previous byte was a CR, whose LF (if next) belongs to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnd {
    Open,
    AfterCarriageReturn,
}

/// Whether the first line, which may start with a byte order mark, is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    FirstLine,
    Later,
}

/// An incremental event-stream parser.
///
/// Feed it bytes as they arrive with [`SseDecoder::feed`]. After an error the
/// decoder is spent: every later feed yields that error once. Drop it and
/// reconnect.
#[derive(Debug)]
pub struct SseDecoder {
    max_line: usize,
    max_event: usize,
    line: Vec<u8>,
    name: String,
    payload: Option<String>,
    line_end: LineEnd,
    position: Position,
    failure: Option<SseError>,
}

impl SseDecoder {
    #[must_use]
    pub const fn new(limits: &WireLimits) -> Self {
        Self {
            max_line: limits.max_sse_line_bytes,
            max_event: limits.max_sse_event_bytes,
            line: Vec::new(),
            name: String::new(),
            payload: None,
            line_end: LineEnd::Open,
            position: Position::FirstLine,
            failure: None,
        }
    }

    /// The events `bytes` completes, in order. The iterator consumes `bytes`
    /// as it is driven; drive it to the end before feeding more.
    #[must_use = "bytes are consumed only as the iterator is driven"]
    pub const fn feed<'bytes>(&'bytes mut self, bytes: &'bytes [u8]) -> Events<'bytes> {
        Events {
            decoder: self,
            rest: bytes,
        }
    }

    /// Processes one complete line; `Some` when it dispatches an event.
    fn line(&mut self) -> Result<Option<SseEvent>, SseError> {
        let bytes = core::mem::take(&mut self.line);
        let text = core::str::from_utf8(&bytes).map_err(|_utf8| SseError::NotUtf8)?;
        let text = match self.position {
            Position::FirstLine => {
                self.position = Position::Later;
                text.strip_prefix('\u{FEFF}').unwrap_or(text)
            }
            Position::Later => text,
        };
        if text.is_empty() {
            let name = core::mem::take(&mut self.name);
            let dispatched = self.payload.take().map(|payload| SseEvent {
                name: if name.is_empty() {
                    DEFAULT_EVENT.to_owned()
                } else {
                    name
                },
                payload,
            });
            self.reuse(bytes);
            return Ok(dispatched);
        }
        if !text.starts_with(':') {
            let (field, value) = text.split_once(':').unwrap_or((text, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => value.clone_into(&mut self.name),
                "data" => self.push_data(value)?,
                _ => {}
            }
        }
        self.reuse(bytes);
        Ok(None)
    }

    /// Keeps the line buffer's allocation.
    fn reuse(&mut self, mut bytes: Vec<u8>) {
        bytes.clear();
        self.line = bytes;
    }

    fn push_data(&mut self, value: &str) -> Result<(), SseError> {
        let length = match &self.payload {
            Some(payload) => payload.len().saturating_add(1).saturating_add(value.len()),
            None => value.len(),
        };
        if length > self.max_event {
            return Err(SseError::EventTooLarge {
                limit: self.max_event,
            });
        }
        match &mut self.payload {
            Some(payload) => {
                payload.push('\n');
                payload.push_str(value);
            }
            None => self.payload = Some(value.to_owned()),
        }
        Ok(())
    }

    /// Consumes bytes from `rest` up to and including the next dispatched event.
    fn next_event(&mut self, rest: &mut &[u8]) -> Result<Option<SseEvent>, SseError> {
        loop {
            if self.line_end == LineEnd::AfterCarriageReturn {
                match rest.split_first() {
                    None => return Ok(None),
                    Some((&b'\n', tail)) => *rest = tail,
                    Some(_) => {}
                }
                self.line_end = LineEnd::Open;
            }
            let Some(terminator) = rest.iter().position(|&byte| byte == b'\n' || byte == b'\r')
            else {
                self.extend_line(rest)?;
                *rest = &[];
                return Ok(None);
            };
            let (segment, tail) = rest.split_at(terminator);
            self.extend_line(segment)?;
            if let Some((&byte, after)) = tail.split_first() {
                self.line_end = if byte == b'\r' {
                    LineEnd::AfterCarriageReturn
                } else {
                    LineEnd::Open
                };
                *rest = after;
            }
            if let Some(event) = self.line()? {
                return Ok(Some(event));
            }
        }
    }

    fn extend_line(&mut self, segment: &[u8]) -> Result<(), SseError> {
        if self.line.len().saturating_add(segment.len()) > self.max_line {
            return Err(SseError::LineTooLong {
                limit: self.max_line,
            });
        }
        self.line.extend_from_slice(segment);
        Ok(())
    }
}

/// The events one [`SseDecoder::feed`] completes.
#[derive(Debug)]
pub struct Events<'bytes> {
    decoder: &'bytes mut SseDecoder,
    rest: &'bytes [u8],
}

impl Iterator for Events<'_> {
    type Item = Result<SseEvent, SseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(failure) = &self.decoder.failure {
            if self.rest.is_empty() {
                return None;
            }
            self.rest = &[];
            return Some(Err(*failure));
        }
        match self.decoder.next_event(&mut self.rest) {
            Ok(event) => event.map(Ok),
            Err(error) => {
                self.rest = &[];
                self.decoder.failure = Some(error);
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn read(text: &str) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new(&WireLimits::DEFAULT);
        decoder
            .feed(text.as_bytes())
            .collect::<Result<_, _>>()
            .expect("a valid stream")
    }

    fn event(name: &str, payload: &str) -> SseEvent {
        SseEvent::new(name.to_owned(), payload.to_owned()).expect("a valid event")
    }

    #[test]
    fn reads_named_events() {
        let text = "event: data\ndata: [1]\n\nevent: control\ndata: {\"a\":1}\n\n";
        assert_eq!(
            read(text),
            [event("data", "[1]"), event("control", "{\"a\":1}")]
        );
    }

    #[test]
    fn joins_multi_line_data_and_handles_every_line_end() {
        let text = "event: data\r\ndata: [\rdata: 1\ndata: ]\r\n\r";
        assert_eq!(read(text), [event("data", "[\n1\n]")]);
    }

    #[test]
    fn defaults_the_name_and_skips_comments_empty_events_and_a_bom() {
        let text = "\u{FEFF}: keep-alive\n\nevent: nothing\n\ndata:x\nid: 7\nretry: 1\n\ndata\n\n";
        assert_eq!(read(text), [event("message", "x"), event("message", "")]);
    }

    #[test]
    fn an_unterminated_event_is_not_dispatched() {
        assert_eq!(read("event: data\ndata: [1]\n"), []);
    }

    #[test]
    fn enforces_line_and_event_bounds_then_stays_failed() {
        let limits = WireLimits {
            max_sse_line_bytes: 8,
            max_sse_event_bytes: 3,
            ..WireLimits::DEFAULT
        };
        let mut decoder = SseDecoder::new(&limits);
        let events: Vec<_> = decoder.feed(b"data: abcdefgh\n\n").collect();
        assert_eq!(events, [Err(SseError::LineTooLong { limit: 8 })]);
        let again: Vec<_> = decoder.feed(b"data: a\n\n").collect();
        assert_eq!(again, [Err(SseError::LineTooLong { limit: 8 })]);

        let mut decoder = SseDecoder::new(&limits);
        let events: Vec<_> = decoder.feed(b"data: ab\ndata: c\n\n").collect();
        assert_eq!(events, [Err(SseError::EventTooLarge { limit: 3 })]);
    }

    #[test]
    fn rejects_events_that_would_not_round_trip() {
        assert_eq!(
            SseEvent::new(String::new(), String::new()),
            Err(SseError::BadName)
        );
        assert_eq!(
            SseEvent::new("a\nb".to_owned(), String::new()),
            Err(SseError::BadName)
        );
        assert_eq!(
            SseEvent::new("a".to_owned(), "x\ry".to_owned()),
            Err(SseError::BadData)
        );
    }

    /// Decodes `bytes` fed in pieces cut at `cuts`; the events until the first
    /// error, then that error.
    fn decode_split(
        bytes: &[u8],
        cuts: &[usize],
        limits: &WireLimits,
    ) -> (Vec<SseEvent>, Option<SseError>) {
        let mut decoder = SseDecoder::new(limits);
        let mut events = Vec::new();
        let mut start = 0;
        let mut ends: Vec<usize> = cuts
            .iter()
            .map(|cut| cut.checked_rem(bytes.len().saturating_add(1)).unwrap_or(0))
            .collect();
        ends.sort_unstable();
        ends.push(bytes.len());
        for end in ends {
            let piece = bytes.get(start..end).unwrap_or_default();
            start = end;
            for result in decoder.feed(piece) {
                match result {
                    Ok(event) => events.push(event),
                    Err(error) => return (events, Some(error)),
                }
            }
        }
        (events, None)
    }

    fn arbitrary_event() -> impl Strategy<Value = SseEvent> {
        ("[a-z]{1,8}", "[^\r]{0,40}").prop_map(|(name, payload)| event(&name, &payload))
    }

    proptest! {
        #[test]
        fn encoded_events_decode_to_themselves_however_split(
            events in prop::collection::vec(arbitrary_event(), 0..8),
            cuts in prop::collection::vec(any::<usize>(), 0..8),
        ) {
            let mut wire = String::from(KEEP_ALIVE);
            for event in &events {
                event.encode_into(&mut wire);
            }
            let (decoded, error) = decode_split(wire.as_bytes(), &cuts, &WireLimits::DEFAULT);
            prop_assert_eq!(error, None);
            prop_assert_eq!(decoded, events);
        }

        #[test]
        fn splitting_never_changes_the_outcome(
            bytes in prop::collection::vec(
                prop_oneof![
                    Just(b'\n'), Just(b'\r'), Just(b':'), Just(b' '),
                    Just(b'd'), Just(b'a'), Just(b't'), Just(b'e'),
                    Just(0xEF), Just(0xBB), Just(0xBF), Just(0xFF), any::<u8>(),
                ],
                0..200,
            ),
            cuts in prop::collection::vec(any::<usize>(), 0..8),
            max_line in 1usize..16,
            max_event in 0usize..16,
        ) {
            let limits = WireLimits {
                max_sse_line_bytes: max_line,
                max_sse_event_bytes: max_event,
                ..WireLimits::DEFAULT
            };
            prop_assert_eq!(
                decode_split(&bytes, &cuts, &limits),
                decode_split(&bytes, &[], &limits)
            );
        }
    }
}
