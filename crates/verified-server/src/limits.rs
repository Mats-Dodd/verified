//! Every bound the daemon runs under.
//!
//! Parsed once in main and passed in as data. The core's [`Limits`] and the wire's [`WireLimits`] ride along, so
//! one value sizes every queue, buffer, file read and timeout here.

use verified_domain::{Limits, Millis};
use verified_wire::WireLimits;

use crate::payload::ToolArgs;

/// The daemon's bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLimits {
    /// What the core enforces: turns, steps, calls, context, retries, group commit.
    pub core: Limits,
    /// What the wire codecs enforce: bodies, SSE lines and events.
    pub wire: WireLimits,
    /// Sessions one daemon hosts.
    pub sessions: usize,
    /// Capacity of each session's inbox (commands, reads and completions).
    pub session_inbox: usize,
    /// Capacity of the registry's inbox.
    pub registry_inbox: usize,
    /// Capacity of the queue from a model stream to its group-commit batcher.
    pub model_parts_queue: usize,
    /// Most streamed parts one model call may produce before it is failed.
    pub model_parts_per_call: usize,
    /// `max_tokens` sent with every model request.
    pub model_max_tokens: u32,
    /// Largest error body read from a failed model response.
    pub model_error_body_bytes: usize,
    /// Longest a model call waits to dial and handshake (TCP and TLS).
    pub model_connect_timeout: Millis,
    /// Longest a model call waits from sending the request to the
    /// response's status line and headers.
    pub model_response_timeout: Millis,
    /// Longest a model response may go without a body frame (Anthropic
    /// sends `ping` events while it thinks).
    pub model_idle_timeout: Millis,
    /// Longest one model call may take altogether, pings or not: past it the
    /// call fails as a network failure (retried on the core's schedule).
    pub model_total_timeout: Millis,
    /// Longest framed log line: an append refuses a longer one, and replay
    /// treats a longer one as damage. [`ServerLimits::check`] makes sure every
    /// record the daemon makes fits ([`ServerLimits::record_budget`]).
    pub log_line_bytes: usize,
    /// Largest log file: an append past it is refused, and recovery refuses
    /// a larger file. Storage is the world: once an append is refused the
    /// session fails stop (L3) and takes no more records.
    pub log_file_bytes: u64,
    /// Largest file the `read` tool or an `edit` reads.
    pub file_bytes: u64,
    /// Most directory entries a listing returns.
    pub dir_entries: usize,
    /// Most bytes of tool output kept (the tail survives for `bash`).
    pub tool_output_bytes: usize,
    /// How long `bash` may run before its process group is killed.
    pub exec_timeout: Millis,
    /// How long a stopped command's output is still read: past it, a
    /// process that left the group and holds the output cannot hang the
    /// call.
    pub exec_drain: Millis,
    /// How long one live SSE response stays open before the client reconnects.
    pub sse_lifetime: Millis,
    /// Idle time before a live SSE response sends a keep-alive comment.
    pub sse_keep_alive: Millis,
    /// Capacity of the queue feeding one live SSE response.
    pub sse_queue: usize,
    /// Live SSE responses one session serves at once.
    pub sse_followers_per_session: usize,
    /// Live SSE responses the daemon serves at once, across sessions.
    pub sse_followers: usize,
    /// Bytes of records and projected chunks (as JSON) one session holds in
    /// memory. Checked when a message would start a turn: past it the
    /// session is full. A turn already running may finish past it, by at
    /// most one turn, which the core's per-turn bounds cap.
    pub session_bytes: usize,
    /// Largest rules file (`.verified/v.toml`) read; a larger one grants
    /// nothing.
    pub rules_file_bytes: u64,
    /// Most rules one rules file may hold; one with more grants nothing.
    pub rules_count: usize,
    /// How much longer than a run's own bound v waits for its report
    /// before it stops waiting and records `Unknown` (B2). A `read` or an
    /// `edit` has no bound of its own: this is its deadline.
    pub deadline_slack: Millis,
    /// The deadline of a delivery between actors (a `send`, a `spawn`):
    /// the recipient's append, bounded. A stop has none: it is sent until
    /// it lands (O2).
    pub deliver_deadline: Millis,
    /// How long a background `bash` may run before its process group is
    /// killed: its own timeout, as `exec_timeout` is a foreground one's.
    pub task_timeout: Millis,
}

/// Bytes of a log line around a record's payloads: the frame header, the
/// record's tag, ids and fields, and the JSON punctuation.
pub const RECORD_OVERHEAD: usize = 1024;

/// Bytes a tool's output may carry past `tool_output_bytes`: the notes on
/// omitted bytes and how the call ended.
pub const OUTPUT_TRAILER: usize = 1024;

/// How much larger a record's projected chunks may be than its log line:
/// each chunk carries one of the record's payloads, escaped the same way,
/// and its keys and ids at most double the record's. Tiny records' chunks
/// may be larger still, within [`RECORD_OVERHEAD`].
const PROJECTION_GROWTH: usize = 2;

/// JSON escapes a byte into at most six (`\u00XX`).
const ESCAPED: usize = 6;

/// Why a [`ServerLimits`] cannot run a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("limits: {what} needs {needed} bytes, more than {have}")]
pub struct LimitsError {
    pub what: &'static str,
    pub needed: u64,
    pub have: u64,
}

fn fits(what: &'static str, needed: Option<usize>, have: usize) -> Result<(), LimitsError> {
    let have_u64 = u64::try_from(have).unwrap_or(u64::MAX);
    match needed {
        Some(needed) if needed <= have => Ok(()),
        Some(needed) => Err(LimitsError {
            what,
            needed: u64::try_from(needed).unwrap_or(u64::MAX),
            have: have_u64,
        }),
        None => Err(LimitsError {
            what,
            needed: u64::MAX,
            have: have_u64,
        }),
    }
}

impl ServerLimits {
    /// The payload bytes one log record may carry: the line bound less
    /// [`RECORD_OVERHEAD`].
    #[must_use]
    pub const fn record_budget(&self) -> usize {
        self.log_line_bytes.saturating_sub(RECORD_OVERHEAD)
    }

    /// Checks that every record the daemon makes fits a log line, so an
    /// append never refuses one: a user's message and a tool's output at
    /// their bounds, escaped. Model output has no bound of its own small
    /// enough; the shell splits it into batches within
    /// [`ServerLimits::record_budget`] and fails a call whose single part
    /// or whose calls do not fit (`ModelFault::TooLarge`). And that any record's
    /// projection fits one stream read, so a client can follow every
    /// record.
    ///
    /// # Errors
    /// A bound that does not fit, or a log file smaller than one line.
    pub fn check(&self) -> Result<(), LimitsError> {
        let budget = self.record_budget();
        fits(
            "a message at its bound, escaped",
            self.wire.max_message_text_bytes.checked_mul(ESCAPED),
            budget,
        )?;
        fits(
            "a tool's output at its bound, escaped",
            self.tool_output_bytes
                .checked_add(OUTPUT_TRAILER)
                .and_then(|bytes| bytes.checked_mul(ESCAPED)),
            budget,
        )?;
        fits(
            "a record's frame",
            RECORD_OVERHEAD.checked_add(1),
            self.log_line_bytes,
        )?;
        fits(
            "a record's projection in one stream read",
            self.log_line_bytes
                .checked_mul(PROJECTION_GROWTH)
                .and_then(|bytes| bytes.checked_add(RECORD_OVERHEAD)),
            self.wire.max_data_bytes(),
        )?;
        let file = usize::try_from(self.log_file_bytes).unwrap_or(usize::MAX);
        fits("one log line", Some(self.log_line_bytes), file)
    }

    /// When v stops waiting for a run of `args` and records `Unknown`: past
    /// the run's own timeout and drains, so a run's honest report normally
    /// comes first (B2). `None` for a run that reports at once (`tasks`)
    /// or is a timer (`wake`), and for a `cancel`, which the core does.
    /// A stop is no call and has none.
    #[must_use]
    pub const fn deadline(&self, args: &ToolArgs) -> Option<Millis> {
        let drains = self.exec_drain.get().saturating_mul(2);
        let slack = self.deadline_slack.get();
        match args {
            ToolArgs::Read { .. } | ToolArgs::Edit { .. } => Some(self.deadline_slack),
            ToolArgs::Bash { background, .. } => {
                let timeout = if *background {
                    self.task_timeout
                } else {
                    self.exec_timeout
                };
                Some(Millis::new(
                    timeout.get().saturating_add(drains).saturating_add(slack),
                ))
            }
            ToolArgs::Spawn { .. } | ToolArgs::Send { .. } => Some(self.deliver_deadline),
            ToolArgs::Wake { .. } | ToolArgs::Tasks | ToolArgs::Cancel { .. } => None,
        }
    }

    /// The bounds v0 ships with.
    #[must_use]
    pub fn v0() -> Self {
        Self {
            core: Limits::v0(),
            wire: WireLimits::DEFAULT,
            sessions: 256,
            session_inbox: 256,
            registry_inbox: 64,
            model_parts_queue: 256,
            model_parts_per_call: 65_536,
            model_max_tokens: 8_192,
            model_error_body_bytes: 64 << 10,
            model_connect_timeout: Millis::new(10_000),
            model_response_timeout: Millis::new(60_000),
            model_idle_timeout: Millis::new(90_000),
            model_total_timeout: Millis::new(900_000),
            log_line_bytes: 7 << 18,
            log_file_bytes: 1 << 30,
            file_bytes: 4 << 20,
            dir_entries: 1_000,
            tool_output_bytes: 64 << 10,
            exec_timeout: Millis::new(120_000),
            exec_drain: Millis::new(1_000),
            sse_lifetime: Millis::new(60_000),
            sse_keep_alive: Millis::new(15_000),
            sse_queue: 16,
            sse_followers_per_session: 8,
            sse_followers: 64,
            session_bytes: 64 << 20,
            rules_file_bytes: 64 << 10,
            rules_count: 256,
            deadline_slack: Millis::new(30_000),
            deliver_deadline: Millis::new(60_000),
            task_timeout: Millis::new(1_800_000),
        }
    }
}
