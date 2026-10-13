//! The tools: `read` (Safe), `edit` and `bash` (Unsafe).
//!
//! A tool runs an approved call against the [`Env`] and reports what
//! actually happened as an [`Exit`] and a bounded [`ToolOutput`]. The
//! cancel token is checked at safe points: before any side effect a
//! cancelled call is `Interrupted`; file operations that started finish;
//! `bash` has its process group killed.
//!
//! `edit` is a compare-and-swap of `old` for `new` in memory: `old` must
//! appear exactly once in the text read, and the result is written
//! atomically ([`Env::write`]), so a crash never leaves a torn file. Nothing
//! stops another process, or a concurrent edit, from changing the file
//! between the read and the write; the last write wins. An edit is never
//! rerun: after a crash its outcome is unknown.
//!
//! `bash` reports a command it stopped (cancelled or timed out) with one
//! honest message: its process group was killed, but processes it started
//! may still be running.

use core::fmt::Write as _;
use std::collections::VecDeque;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_core::{Approved, ToolCall};
use verified_domain::id::SessionId;
use verified_domain::{Cause, Exit, Fault};

use crate::env::{
    BashGrant, EditGrant, Env, EnvError, ExecOptions, ExitStatus, Grant, Stopped, Written,
};
use crate::limits::ServerLimits;
use crate::payload::{Parsed, ProjectPath, ToolOutput, Truncation};

/// The environment variable `bash` commands see their call id in.
pub const CALL_ID_VAR: &str = "VERIFIED_CALL_ID";

const fn failed() -> Exit {
    Exit::Err(Cause::Failed(Fault::Other))
}

const fn failure(text: String) -> (Exit, ToolOutput) {
    (failed(), ToolOutput::complete(text))
}

/// Keeps the first `limit` bytes of `text`, cut at a char boundary.
#[must_use]
pub fn bounded_head(text: &str, limit: usize) -> ToolOutput {
    if text.len() <= limit {
        return ToolOutput::complete(text.to_owned());
    }
    let cut = (0..=limit)
        .rev()
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or_default();
    let kept = text.get(..cut).unwrap_or_default();
    let omitted = u64::try_from(text.len().saturating_sub(cut)).unwrap_or(u64::MAX);
    ToolOutput {
        text: format!("{kept}\n[… {omitted} more bytes not shown]"),
        truncation: Truncation::Truncated {
            omitted_bytes: omitted,
        },
    }
}

/// Runs an approved call. Never panics on tool failure: every failure is an
/// `Exit` the model hears about. What the call may touch comes from the call
/// itself ([`Grant::of`]).
pub async fn run(
    env: &dyn Env,
    session: SessionId,
    call: &ToolCall<Approved, Parsed>,
    cancel: &CancellationToken,
    limits: &ServerLimits,
) -> (Exit, ToolOutput) {
    if cancel.is_cancelled() {
        return (
            Exit::Err(Cause::Interrupted),
            ToolOutput::complete("Interrupted before it started.".to_owned()),
        );
    }
    match Grant::of(call, session) {
        Grant::Read(path) => read(env, path, limits).await,
        Grant::Edit(grant) => edit(env, &grant, limits).await,
        Grant::Bash(grant) => bash(env, &grant, cancel, limits).await,
    }
}

async fn read(env: &dyn Env, path: &ProjectPath, limits: &ServerLimits) -> (Exit, ToolOutput) {
    match env.read(path, limits.file_bytes).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => (Exit::Ok, bounded_head(&text, limits.tool_output_bytes)),
            Err(_) => failure(format!("{path} is not UTF-8 text")),
        },
        Err(EnvError::IsDirectory { .. }) => match env.list(Some(path), limits.dir_entries).await {
            Ok(entries) => {
                let mut listing = String::new();
                for entry in entries {
                    listing.push_str(&entry.name);
                    if matches!(entry.kind, crate::env::EntryKind::Dir) {
                        listing.push('/');
                    }
                    listing.push('\n');
                }
                (Exit::Ok, bounded_head(&listing, limits.tool_output_bytes))
            }
            Err(error) => failure(error.to_string()),
        },
        Err(error) => failure(error.to_string()),
    }
}

/// Compare in memory, then one atomic write: `old` must appear exactly
/// once in the text read.
async fn edit(env: &dyn Env, grant: &EditGrant<'_>, limits: &ServerLimits) -> (Exit, ToolOutput) {
    let (path, old, new) = (grant.path(), grant.old(), grant.replacement());
    let text = match env.read(path, limits.file_bytes).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return failure(format!("{path} is not UTF-8 text")),
        },
        Err(error) => return failure(error.to_string()),
    };
    match text.match_indices(old).take(2).count() {
        1 => {}
        0 => return failure(format!("The old text was not found in {path}.")),
        _ => {
            return failure(format!(
                "The old text appears more than once in {path}; include more context."
            ));
        }
    }
    match env
        .write(grant, text.replacen(old, new, 1).as_bytes())
        .await
    {
        Ok(Written::Synced) => (Exit::Ok, ToolOutput::complete(format!("Edited {path}."))),
        Ok(Written::Unsynced { reason }) => (
            Exit::Ok,
            ToolOutput::complete(format!(
                "Edited {path}, but syncing its directory failed ({reason}): a crash may undo the edit."
            )),
        ),
        Err(error) => failure(error.to_string()),
    }
}

/// What `bash` says about a command it stopped.
#[must_use]
pub const fn stopped(why: Stopped) -> &'static str {
    match why {
        Stopped::Cancelled => "stopped (cancelled); processes it started may still be running",
        Stopped::TimedOut => "stopped (timed out); processes it started may still be running",
    }
}

/// The tail of a command's output, at most `limit` bytes.
struct Kept {
    bytes: VecDeque<u8>,
    omitted: u64,
    limit: usize,
}

impl Kept {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        while self.bytes.len() > self.limit {
            self.bytes.pop_front();
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

async fn collect(mut output: mpsc::Receiver<Vec<u8>>, limit: usize) -> Kept {
    let mut tail = Kept {
        bytes: VecDeque::with_capacity(limit),
        omitted: 0,
        limit,
    };
    while let Some(chunk) = output.recv().await {
        tail.push(&chunk);
    }
    tail
}

async fn bash(
    env: &dyn Env,
    grant: &BashGrant<'_>,
    cancel: &CancellationToken,
    limits: &ServerLimits,
) -> (Exit, ToolOutput) {
    let options = ExecOptions {
        timeout: limits.exec_timeout,
        drain: limits.exec_drain,
        env: vec![(CALL_ID_VAR.to_owned(), grant.id().as_str().to_owned())],
    };
    let (sender, receiver) = mpsc::channel(limits.exec_output_queue);
    let (status, tail) = tokio::join!(
        env.exec(grant, &options, sender, cancel),
        collect(receiver, limits.tool_output_bytes),
    );
    let status = match status {
        Ok(status) => status,
        Err(error) => return failure(error.to_string()),
    };
    let mut text = String::new();
    if tail.omitted > 0 {
        writeln!(text, "[… {} earlier bytes not shown]", tail.omitted)
            .expect("writing to a String never fails");
    }
    let contiguous: Vec<u8> = tail.bytes.iter().copied().collect();
    text.push_str(&String::from_utf8_lossy(&contiguous));
    let (exit, trailer) = match status {
        ExitStatus::Code(0i32) => (Exit::Ok, "[exit status 0]".to_owned()),
        ExitStatus::Code(code) => (failed(), format!("[exit status {code}]")),
        ExitStatus::Signal(signal) => (failed(), format!("[killed by signal {signal}]")),
        ExitStatus::Stopped(why) => (
            match why {
                Stopped::Cancelled => Exit::Err(Cause::Interrupted),
                Stopped::TimedOut => failed(),
            },
            format!("[{}]", stopped(why)),
        ),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&trailer);
    let truncation = if tail.omitted > 0 {
        Truncation::Truncated {
            omitted_bytes: tail.omitted,
        }
    } else {
        Truncation::Complete
    };
    (exit, ToolOutput { text, truncation })
}
