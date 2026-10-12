//! The tools: `read` (Safe), `edit` (Keyed) and `bash` (Never).
//!
//! A tool runs an approved call against the [`Env`] and reports what
//! actually happened as an [`Exit`] and a bounded [`ToolOutput`]. The
//! cancel token is checked at safe points: before any side effect a
//! cancelled call is `Interrupted`; file operations that started finish;
//! `bash` has its process group killed.
//!
//! `edit` is a compare-and-swap of `old` for `new`: `old` must appear exactly
//! once. It writes a sibling temp file named by the call's key (its session
//! and `Dispatched` seq), makes a durable per-call marker ([`Env::mark`])
//! holding the fingerprint of the text it read, and renames the temp file
//! over the target, so a crash never leaves a torn file. Rerun after a crash
//! (same key), the marker says how far it got: marked with the temp file
//! still there, it finishes the rename if the file still holds the text the
//! temp file was made from (otherwise it fails and says why); marked
//! without it, it reports "already applied". The file's text alone is never
//! the evidence: `new` may be empty (a deletion), may contain `old`, or may
//! already appear.

use core::fmt::Write as _;
use std::collections::VecDeque;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_core::{Approved, ToolCall};
use verified_domain::id::SessionId;
use verified_domain::{Cause, Exit, Fault};

use crate::env::{
    BashGrant, EditGrant, Env, EnvError, ExecOptions, ExitStatus, Fingerprint, Grant, Stat, Stopped,
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

async fn edit(env: &dyn Env, grant: &EditGrant<'_>, limits: &ServerLimits) -> (Exit, ToolOutput) {
    let path = grant.path();
    match env.marked(grant).await {
        Err(error) => failure(error.to_string()),
        Ok(None) => edit_fresh(env, grant, limits).await,
        Ok(Some(original)) => match env.stat(grant.staged()).await {
            // Marked, not yet renamed: the rename finishes the edit, but only
            // over the text the staged copy was made from.
            Ok(Stat::File { .. }) => {
                // The staged copy and the marker stay either way: a rerun
                // must reach the same verdict, and both are removed once the
                // result is durable.
                let now = match env.read(path, limits.file_bytes).await {
                    Ok(bytes) => Fingerprint::of(&bytes),
                    Err(error) => return failure(error.to_string()),
                };
                if now != original {
                    return failure(format!(
                        "{path} changed between staging this edit and finishing it after a restart; the edit was not applied."
                    ));
                }
                match env.commit(grant).await {
                    Ok(()) => (Exit::Ok, ToolOutput::complete(format!("Edited {path}."))),
                    Err(error) => failure(error.to_string()),
                }
            }
            Ok(Stat::Missing) => (
                Exit::Ok,
                ToolOutput::complete(format!("Already applied: {path} was edited before.")),
            ),
            Ok(Stat::Dir | Stat::Symlink | Stat::Other) => failure(format!(
                "{} is not the staged file this edit wrote",
                grant.staged()
            )),
            Err(error) => failure(error.to_string()),
        },
    }
}

/// An edit that never reached its rename: compare, stage, mark, rename.
async fn edit_fresh(
    env: &dyn Env,
    grant: &EditGrant<'_>,
    limits: &ServerLimits,
) -> (Exit, ToolOutput) {
    let (path, old, new) = (grant.path(), grant.old(), grant.replacement());
    let (text, original) = match env.read(path, limits.file_bytes).await {
        Ok(bytes) => {
            let original = Fingerprint::of(&bytes);
            match String::from_utf8(bytes) {
                Ok(text) => (text, original),
                Err(_) => return failure(format!("{path} is not UTF-8 text")),
            }
        }
        Err(error) => return failure(error.to_string()),
    };
    let found = text.match_indices(old).take(2).count();
    match found {
        1 => {}
        0 => return failure(format!("The old text was not found in {path}.")),
        _ => {
            return failure(format!(
                "The old text appears more than once in {path}; include more context."
            ));
        }
    }
    let replaced = text.replacen(old, new, 1);
    if let Err(error) = env.stage(grant, replaced.as_bytes()).await {
        // No marker yet, so nothing vouches for a half-written staged copy:
        // remove it (best effort; a leftover is never renamed without a
        // marker, and is removed once the result is durable).
        return match env.discard(grant).await {
            Ok(()) => failure(error.to_string()),
            Err(also) => failure(format!(
                "{error} (and the staged copy could not be removed: {also})"
            )),
        };
    }
    // From here on a failure leaves the staged copy and any marker as they
    // are: a marker without its staged copy reads as "already applied", so
    // the copy goes only with the marker, once the result is durable.
    if let Err(error) = env.mark(grant, original).await {
        return failure(error.to_string());
    }
    if let Err(error) = env.commit(grant).await {
        return failure(error.to_string());
    }
    (Exit::Ok, ToolOutput::complete(format!("Edited {path}.")))
}

/// The tail of a command's output, at most `limit` bytes.
struct Tail {
    bytes: VecDeque<u8>,
    omitted: u64,
    limit: usize,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        while self.bytes.len() > self.limit {
            self.bytes.pop_front();
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

async fn collect(mut output: mpsc::Receiver<Vec<u8>>, limit: usize) -> Tail {
    let mut tail = Tail {
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
        reap_schedule: limits.exec_reap_schedule.clone(),
        drain: limits.exec_drain,
        env: vec![(CALL_ID_VAR.to_owned(), grant.id().as_str().to_owned())],
    };
    let (sender, receiver) = mpsc::channel(limits.exec_output_queue);
    let (status, tail) = tokio::join!(
        env.exec(grant, &options, sender, cancel),
        collect(receiver, limits.tool_output_bytes),
    );
    let status = match status {
        Ok(status) => Ok(status),
        Err(EnvError::Kill(error)) => Err(error),
        Err(
            error @ (EnvError::NotFound { .. }
            | EnvError::PermissionDenied { .. }
            | EnvError::IsDirectory { .. }
            | EnvError::NotDirectory { .. }
            | EnvError::TooLarge { .. }
            | EnvError::Symlink { .. }
            | EnvError::Spawn { .. }
            | EnvError::Io { .. }),
        ) => return failure(error.to_string()),
    };
    let mut text = String::new();
    if tail.omitted > 0 {
        writeln!(text, "[… {} earlier bytes not shown]", tail.omitted)
            .expect("writing to a String never fails");
    }
    let contiguous: Vec<u8> = tail.bytes.iter().copied().collect();
    text.push_str(&String::from_utf8_lossy(&contiguous));
    let (exit, trailer) = match status {
        // Never claim a kill that failed: the command may still be running,
        // so what it did is unknown.
        Err(error) => (
            Exit::Err(Cause::Unknown),
            format!("[{error}; it may still be running]"),
        ),
        Ok(ExitStatus::Code(0i32)) => (Exit::Ok, "[exit status 0]".to_owned()),
        Ok(ExitStatus::Code(code)) => (failed(), format!("[exit status {code}]")),
        Ok(ExitStatus::Signal) => (failed(), "[killed by a signal]".to_owned()),
        Ok(ExitStatus::Killed(Stopped::Cancelled)) => (
            Exit::Err(Cause::Interrupted),
            "[interrupted: the process group was killed]".to_owned(),
        ),
        Ok(ExitStatus::Killed(Stopped::TimedOut)) => (
            failed(),
            format!(
                "[timed out after {} ms: the process group was killed]",
                limits.exec_timeout.get()
            ),
        ),
        // The group is dead, but a process it started left the group and
        // kept the output open: it may still be running.
        Ok(ExitStatus::Escaped(why)) => (
            Exit::Err(Cause::Unknown),
            format!(
                "[{}: the process group was stopped, but a process that left it still holds the output; it may still be running]",
                match why {
                    Stopped::Cancelled => "interrupted",
                    Stopped::TimedOut => "timed out",
                }
            ),
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
