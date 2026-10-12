//! The bounds every decoder and fold in this crate enforces.
//!
//! The daemon and the TUI derive one `WireLimits` from the `Limits` value
//! parsed at startup and pass it in. Exceeding a bound is a typed error at the
//! point of decoding, never silent growth.

/// Size bounds for wire decoding and the transcript fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireLimits {
    /// Largest command body (a POST) accepted, in bytes.
    pub max_body_bytes: usize,
    /// Longest user message text, in bytes.
    pub max_message_text_bytes: usize,
    /// Longest SSE line, in bytes, excluding its terminator.
    pub max_sse_line_bytes: usize,
    /// Largest SSE event data (all `data:` lines joined), in bytes.
    pub max_sse_event_bytes: usize,
    /// Most chunks one Durable Streams read (an SSE `data` event or a
    /// catch-up body) may carry.
    pub max_chunks_per_read: usize,
    /// Most turns a transcript holds.
    pub max_turns: usize,
    /// Most parts one assistant reply holds.
    pub max_parts_per_reply: usize,
    /// Longest accumulated part text (text, reasoning, raw tool input), in bytes.
    pub max_part_bytes: usize,
    /// Most `error` chunks one turn records.
    pub max_errors_per_turn: usize,
}

impl WireLimits {
    /// Bounds for development and tests; production takes them from `Limits`.
    pub const DEFAULT: Self = Self {
        max_body_bytes: 1 << 20,
        max_message_text_bytes: 256 << 10,
        max_sse_line_bytes: 4 << 20,
        max_sse_event_bytes: 4 << 20,
        max_chunks_per_read: 4096,
        max_turns: 1024,
        max_parts_per_reply: 1024,
        max_part_bytes: 1 << 20,
        max_errors_per_turn: 64,
    };
}
