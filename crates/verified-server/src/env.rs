//! The `Env` seam: the project's files and a shell.
//!
//! Shaped after pi's `ExecutionEnv`
//! (`packages/agent/src/harness/types.ts`). Every method
//! returns a `Result` and never panics; paths are [`ProjectPath`]s, parsed
//! once at the boundary (relative, no `..`, never the daemon's own state).
//! Mutating methods take a grant ([`EditGrant`], [`BashGrant`]) that only
//! [`Grant::of`] makes, from a core-minted `ToolCall<Approved, _>`, and
//! everything a mutation may touch (the file, the command) is read from that
//! call. So running a mutation without a durable approval does not compile,
//! and an approval for one call cannot authorize another call's mutation.
//!
//! v controls dispatch, not the filesystem or the OS: an `Env` acts on the
//! world best effort and reports what happened honestly. Containment is a
//! future sandbox `Env`.

use core::fmt;
use core::pin::Pin;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;
use verified_core::{Approved, ToolCall};
use verified_domain::id::{ActorId, CallId};
use verified_domain::{EffectId, Millis};

use crate::payload::{Command, Parsed, ProjectPath, Sibling, ToolArgs};
use crate::seam::{Clock, ProcessGroups};
use crate::shim::fs;

/// Why an `Env` operation failed, backend-independent (pi's `FileErrorCode`
/// and `ExecutionErrorCode`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvError {
    #[error("{path}: not found")]
    NotFound { path: String },
    #[error("{path}: permission denied")]
    PermissionDenied { path: String },
    #[error("{path}: is a directory")]
    IsDirectory { path: String },
    #[error("{path}: not a directory")]
    NotDirectory { path: String },
    #[error("{path}: {length} bytes, more than {limit}")]
    TooLarge {
        path: String,
        length: u64,
        limit: u64,
    },
    #[error("the shell could not start: {reason}")]
    Spawn { reason: String },
    #[error("{path}: {reason}")]
    Io { path: String, reason: String },
}

impl EnvError {
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "io::ErrorKind is non_exhaustive; every other kind is a plain IO error"
    )]
    fn from_io(path: &str, error: &io::Error) -> Self {
        let path = path.to_owned();
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound { path },
            io::ErrorKind::PermissionDenied => Self::PermissionDenied { path },
            io::ErrorKind::IsADirectory => Self::IsDirectory { path },
            io::ErrorKind::NotADirectory => Self::NotDirectory { path },
            _ => Self::Io {
                path,
                reason: error.to_string(),
            },
        }
    }
}

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// Why a command was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// Its cancel token fired.
    Cancelled,
    /// It ran past its timeout.
    TimedOut,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// It exited with this code.
    Code(i32),
    /// A signal ended it. v signals a command only to stop it, so this
    /// came from elsewhere (the command itself, the OS, a user, or the
    /// daemon's own crash-time kill of every group).
    Signal(i32),
    /// We stopped it: its process group was sent `SIGKILL`. Processes it
    /// started may still be running (one that left the group, or one the
    /// kill could not reach).
    Stopped(Stopped),
}

/// A write that replaced the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// The rename is durable: its directory is synced.
    Synced,
    /// The file holds the new contents, but syncing its directory failed,
    /// so a crash may still undo the rename.
    Unsynced { reason: String },
}

/// How to run a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOptions {
    pub timeout: Millis,
    /// How long output is still read once the group is stopped.
    pub drain: Millis,
    /// How many of the last bytes of output are kept.
    pub output_bytes: usize,
    /// Extra environment variables (e.g. `VERIFIED_CALL_ID`).
    pub env: Vec<(String, String)>,
}

/// The tail of a command's output: its last `limit` bytes, and how many
/// came before them.
#[derive(Debug)]
pub struct Kept {
    pub(crate) bytes: VecDeque<u8>,
    pub(crate) omitted: u64,
    limit: usize,
}

impl Kept {
    /// Keeps nothing yet, and at most `limit` bytes.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(limit),
            omitted: 0,
            limit,
        }
    }

    /// Appends `chunk`, dropping the oldest bytes past the limit.
    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        while self.bytes.len() > self.limit {
            self.bytes.pop_front();
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

/// What an approved call may do, derived from the call alone.
#[derive(Debug)]
pub enum Grant<'call> {
    /// Read a file or list a directory: no mutation.
    Read(&'call ProjectPath),
    Edit(EditGrant<'call>),
    Bash(BashGrant<'call>),
}

/// An approved `edit`: replace `old` by `new` in `path`.
#[derive(Debug)]
pub struct EditGrant<'call> {
    id: &'call CallId,
    effect: EffectId,
    path: &'call ProjectPath,
    old: &'call str,
    new: &'call str,
    staged: Sibling,
}

/// An approved `bash`: run `command`.
#[derive(Debug, Clone, Copy)]
pub struct BashGrant<'call> {
    id: &'call CallId,
    effect: EffectId,
    command: &'call Command,
}

impl<'call> Grant<'call> {
    /// What `call`, of `session`, may do to the world; `None` for a call
    /// that acts only on the runtime (`spawn`, `send`, `wake`, `tasks`,
    /// `cancel`). The only way to make an [`EditGrant`] or a [`BashGrant`].
    #[must_use]
    pub fn of(call: &'call ToolCall<Approved, Parsed>, session: ActorId) -> Option<Self> {
        let effect = call.effect();
        match call.args() {
            ToolArgs::Read { path } => Some(Self::Read(path)),
            ToolArgs::Edit { path, old, new } => Some(Self::Edit(EditGrant {
                id: call.id(),
                effect,
                path,
                old,
                new,
                staged: Sibling::staged(session, effect.seq),
            })),
            ToolArgs::Bash { command, .. } => Some(Self::Bash(BashGrant {
                id: call.id(),
                effect,
                command,
            })),
            ToolArgs::Spawn { .. }
            | ToolArgs::Send { .. }
            | ToolArgs::Wake { .. }
            | ToolArgs::Tasks
            | ToolArgs::Cancel { .. } => None,
        }
    }
}

impl EditGrant<'_> {
    /// The approved call.
    #[must_use]
    pub const fn id(&self) -> &CallId {
        self.id
    }

    /// The effect running the call: its epoch and its `Dispatched` seq.
    #[must_use]
    pub const fn effect(&self) -> EffectId {
        self.effect
    }

    /// The file the edit changes.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        self.path
    }

    /// The text the edit replaces.
    #[must_use]
    pub const fn old(&self) -> &str {
        self.old
    }

    /// The text that replaces it.
    #[must_use]
    pub const fn replacement(&self) -> &str {
        self.new
    }

    /// The suffix of the file the new contents are written to before the
    /// rename: `verified-<session>-<seq>`, unique to this call, so
    /// concurrent edits never share one.
    #[must_use]
    pub const fn staged(&self) -> &Sibling {
        &self.staged
    }
}

impl BashGrant<'_> {
    /// The approved call.
    #[must_use]
    pub const fn id(&self) -> &CallId {
        self.id
    }

    /// The effect running the call: its epoch and its `Dispatched` seq.
    #[must_use]
    pub const fn effect(&self) -> EffectId {
        self.effect
    }

    #[must_use]
    pub const fn command(&self) -> &Command {
        self.command
    }
}

/// The Env seam.
#[async_trait]
pub trait Env: Send + Sync + fmt::Debug {
    /// The bytes of the file at `path`, at most `limit`.
    ///
    /// # Errors
    /// Missing, a directory, larger than `limit`, or IO.
    async fn read(&self, path: &ProjectPath, limit: u64) -> Result<Vec<u8>, EnvError>;

    /// The entries of the directory at `path` (`None`: the project root),
    /// sorted, at most `limit`.
    ///
    /// # Errors
    /// Missing, not a directory, too many entries, or IO.
    async fn list(
        &self,
        path: Option<&ProjectPath>,
        limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError>;

    /// Replaces the contents of the edit's file with `contents`, atomically:
    /// written to a sibling named by [`EditGrant::staged`], synced, given the
    /// file's mode, renamed over the file, and the directory synced. An
    /// error leaves the file as it was; once the rename is done the file has
    /// changed, and a failed directory sync is [`Written::Unsynced`], not an
    /// error. The file is a new one: its hard links and ownership are not
    /// kept.
    ///
    /// # Errors
    /// IO before the rename.
    async fn write(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<Written, EnvError>;

    /// Runs the approved command with `sh -c` in the project root, in its
    /// own process group, and keeps the last `ExecOptions::output_bytes` of
    /// its combined output ([`Kept`]). The command runs until its leader
    /// exits, even after its output closes; nothing else in its group is
    /// signalled then, so background processes it started may keep running.
    /// `cancel` and the timeout send the group one `SIGKILL` and make the
    /// result [`ExitStatus::Stopped`]; after that its output is read, and
    /// then its leader waited for, at most `ExecOptions::drain` each, so
    /// neither a process that left the group holding the output nor a leader
    /// the kill could not reach can hang the call.
    ///
    /// # Errors
    /// The shell could not start.
    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        cancel: &CancellationToken,
    ) -> Result<(ExitStatus, Kept), EnvError>;

    /// The bytes of the rules file, `.verified/v.toml` in the project, at
    /// most `limit`; `None` when there is none. The user writes it; no tool
    /// may ([`ProjectPath`] reserves the directory). An environment without
    /// one, by default, has none: its only rule is the default.
    ///
    /// # Errors
    /// Larger than `limit`, not a file, or IO.
    async fn rules_file(&self, limit: u64) -> Result<Option<Vec<u8>>, EnvError> {
        let _: u64 = limit;
        Ok(None)
    }
}

/// An environment with nothing in it: no files, no shell and no rules
/// file. Every tool fails honestly. For a daemon that runs no tools (the
/// HTTP tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEnv;

#[async_trait]
impl Env for NoEnv {
    async fn read(&self, path: &ProjectPath, _limit: u64) -> Result<Vec<u8>, EnvError> {
        Err(EnvError::NotFound {
            path: path.as_str().to_owned(),
        })
    }

    async fn list(
        &self,
        path: Option<&ProjectPath>,
        _limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError> {
        Err(EnvError::NotFound {
            path: path.map_or_else(String::new, |path| path.as_str().to_owned()),
        })
    }

    async fn write(&self, edit: &EditGrant<'_>, _contents: &[u8]) -> Result<Written, EnvError> {
        Err(EnvError::NotFound {
            path: edit.path().as_str().to_owned(),
        })
    }

    async fn exec(
        &self,
        _bash: &BashGrant<'_>,
        _options: &ExecOptions,
        _cancel: &CancellationToken,
    ) -> Result<(ExitStatus, Kept), EnvError> {
        Err(EnvError::Spawn {
            reason: "this environment has no shell".to_owned(),
        })
    }
}

/// The real environment, a best-effort development env: the project
/// directory through the fs shim, and `/bin/sh` through tokio's process
/// support.
///
/// It contains nothing. Symlinks are followed, so a path may reach outside
/// the project through one; only the lexical rules of [`ProjectPath`] hold.
/// A command runs with the daemon's rights, and processes it starts may
/// outlive it (one that leaves its process group escapes every kill).
#[derive(Debug, Clone)]
pub struct LocalEnv {
    root: PathBuf,
    clock: Arc<dyn Clock>,
    groups: ProcessGroups,
}

impl LocalEnv {
    /// The project at `root`; commands it starts are registered in `groups`
    /// (those of the daemon's [`crate::seam::Tasks`], so a crashing daemon
    /// kills them).
    #[must_use]
    pub const fn new(root: PathBuf, clock: Arc<dyn Clock>, groups: ProcessGroups) -> Self {
        Self {
            root,
            clock,
            groups,
        }
    }
}

const fn entry_kind(kind: fs::Kind) -> EntryKind {
    match kind {
        fs::Kind::File => EntryKind::File,
        fs::Kind::Dir => EntryKind::Dir,
        fs::Kind::Symlink => EntryKind::Symlink,
        fs::Kind::Other => EntryKind::Other,
    }
}

/// Sends `SIGKILL` to the process group `group`, best effort: a group that
/// is gone already, or that cannot be signalled, is left as it is.
fn kill_group(group: rustix::process::Pid) {
    let _best_effort: Result<(), rustix::io::Errno> =
        rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
}

/// A running command's process group: the child that leads it, and whether
/// it is reaped.
///
/// Registered in the daemon's [`ProcessGroups`] while its leader is not yet
/// reaped. Dropped before that (the daemon stopping with the command still
/// running), it kills the whole group: `kill_on_drop` alone signals only
/// the leader. Once the leader is reaped the group id may be reused by an
/// unrelated process, so nothing signals it again.
struct Group {
    child: tokio::process::Child,
    /// The group id (the leader's pid), fixed at spawn.
    id: Option<rustix::process::Pid>,
    reaped: bool,
    groups: ProcessGroups,
}

impl Group {
    fn new(child: tokio::process::Child, groups: ProcessGroups) -> Self {
        let id = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw);
        if let Some(id) = id {
            groups.insert(id.as_raw_nonzero().get());
        }
        Self {
            child,
            id,
            reaped: false,
            groups,
        }
    }

    /// Sends `SIGKILL` to the whole group, best effort, while the group id
    /// is still ours (the leader is not reaped).
    fn kill(&self) {
        if !self.reaped
            && let Some(id) = self.id
        {
            kill_group(id);
        }
    }

    /// Waits for the leader and reaps it; the group id is never signalled
    /// again.
    async fn wait(&mut self) -> Result<std::process::ExitStatus, EnvError> {
        let status = self.child.wait().await.map_err(|error| EnvError::Spawn {
            reason: error.to_string(),
        })?;
        self.reaped = true;
        if let Some(id) = self.id {
            self.groups.remove(id.as_raw_nonzero().get());
        }
        Ok(status)
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.reaped
            && let Some(id) = self.id
        {
            // The daemon is stopping, or the command's result is already
            // lost: nothing of it should keep running.
            kill_group(id);
            self.groups.remove(id.as_raw_nonzero().get());
        }
    }
}

/// How a command that nobody stopped ended.
fn exit_status(status: std::process::ExitStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitStatus::Code(code),
        (None, signal) => ExitStatus::Signal(signal.unwrap_or_default()),
    }
}

type Waiting<'wait> = Pin<Box<dyn Future<Output = ()> + Send + 'wait>>;

/// The bytes of the regular file at `at` (named `shown` in errors), at most
/// `limit`.
async fn read_file(at: &Path, shown: &str, limit: u64) -> Result<Vec<u8>, EnvError> {
    let named = || shown.to_owned();
    let found = fs::stat(at)
        .await
        .map_err(|error| EnvError::from_io(shown, &error))?;
    match found {
        None => return Err(EnvError::NotFound { path: named() }),
        Some((fs::Kind::Dir, _)) => return Err(EnvError::IsDirectory { path: named() }),
        Some((fs::Kind::Symlink | fs::Kind::Other, _)) => {
            return Err(EnvError::Io {
                path: named(),
                reason: "not a regular file".to_owned(),
            });
        }
        Some((fs::Kind::File, bytes)) if bytes > limit => {
            return Err(EnvError::TooLarge {
                path: named(),
                length: bytes,
                limit,
            });
        }
        Some((fs::Kind::File, _)) => {}
    }
    let bytes = fs::read(at)
        .await
        .map_err(|error| EnvError::from_io(shown, &error))?;
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if length > limit {
        return Err(EnvError::TooLarge {
            path: named(),
            length,
            limit,
        });
    }
    Ok(bytes)
}

#[async_trait]
impl Env for LocalEnv {
    async fn read(&self, path: &ProjectPath, limit: u64) -> Result<Vec<u8>, EnvError> {
        read_file(&path.under(&self.root), path.as_str(), limit).await
    }

    async fn rules_file(&self, limit: u64) -> Result<Option<Vec<u8>>, EnvError> {
        let shown = format!("{}/{}", crate::daemon::DIR, crate::rules_file::FILE);
        match read_file(&self.root.join(&shown), &shown, limit).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(EnvError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn list(
        &self,
        path: Option<&ProjectPath>,
        limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError> {
        let shown = path.map_or(".", ProjectPath::as_str);
        let dir = path.map_or_else(|| self.root.clone(), |path| path.under(&self.root));
        let entries = fs::list(&dir, limit).await.map_err(|error| {
            if error.kind() == io::ErrorKind::FileTooLarge {
                EnvError::TooLarge {
                    path: shown.to_owned(),
                    length: u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1),
                    limit: u64::try_from(limit).unwrap_or(u64::MAX),
                }
            } else {
                EnvError::from_io(shown, &error)
            }
        })?;
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name.to_string_lossy().into_owned(),
                kind: entry_kind(entry.kind),
            })
            .collect())
    }

    async fn write(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<Written, EnvError> {
        let path = edit.path();
        let failed = |error: io::Error| EnvError::from_io(path.as_str(), &error);
        // A symlink at the path is followed: the file it names is replaced
        // and the link stays a link.
        let target = fs::canonicalize(&path.under(&self.root))
            .await
            .map_err(failed)?;
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
            return Err(EnvError::Io {
                path: path.as_str().to_owned(),
                reason: "not a file".to_owned(),
            });
        };
        let mut staged_name = OsString::from(".");
        staged_name.push(name);
        staged_name.push(".");
        staged_name.push(edit.staged().as_str());
        let staged = dir.join(staged_name);
        let written = match fs::write_synced(&staged, contents).await {
            Ok(()) => match fs::copy_mode(&target, &staged).await {
                Ok(()) => fs::rename(&staged, &target).await,
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            // Best effort: a leftover is never read again.
            drop(fs::remove_file(&staged).await);
            return Err(failed(error));
        }
        // The rename is durable only once its directory is: without this a
        // crash can undo an edit whose result the log already holds. The
        // file has changed either way, so a failure here is no error.
        Ok(match fs::sync_dir(dir).await {
            Ok(()) => Written::Synced,
            Err(error) => Written::Unsynced {
                reason: error.to_string(),
            },
        })
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the Env seam's real impl is the one place a process starts; it owns the process group"
    )]
    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        cancel: &CancellationToken,
    ) -> Result<(ExitStatus, Kept), EnvError> {
        let command = bash.command();
        let mut process = tokio::process::Command::new("/bin/sh");
        process
            .arg("-c")
            .arg(command.as_str())
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for (key, value) in &options.env {
            process.env(key, value);
        }
        let child = process.spawn().map_err(|error| EnvError::Spawn {
            reason: error.to_string(),
        })?;
        let mut group = Group::new(child, self.groups.clone());
        let (Some(mut stdout), Some(mut stderr)) =
            (group.child.stdout.take(), group.child.stderr.take())
        else {
            group.kill();
            return Err(EnvError::Spawn {
                reason: "no pipes".to_owned(),
            });
        };
        let mut kept = Kept::new(options.output_bytes);
        let mut out_buffer = vec![0u8; 8192];
        let mut err_buffer = vec![0u8; 8192];
        let (mut out_open, mut err_open) = (true, true);
        let mut stopped = None;
        let mut timeout = self.clock.sleep(options.timeout);
        // Armed once the group is stopped: output is read for at most this
        // long more, so a process that left the group and holds the pipes
        // cannot hang the call.
        let mut drain: Waiting<'_> = Box::pin(core::future::pending());
        // Stopping comes before reading: a command that writes without
        // pause would otherwise starve the cancel and the timeout.
        while out_open || err_open {
            let running = stopped.is_none();
            let read = tokio::select! {
                biased;
                () = &mut drain => break,
                () = cancel.cancelled(), if running => {
                    stopped = Some(Stopped::Cancelled);
                    group.kill();
                    drain = self.clock.sleep(options.drain);
                    continue;
                }
                () = &mut timeout, if running => {
                    stopped = Some(Stopped::TimedOut);
                    group.kill();
                    drain = self.clock.sleep(options.drain);
                    continue;
                }
                read = stdout.read(&mut out_buffer), if out_open => (Stream::Out, read),
                read = stderr.read(&mut err_buffer), if err_open => (Stream::Err, read),
            };
            let (stream, result) = read;
            let (buffer, open) = match stream {
                Stream::Out => (&out_buffer, &mut out_open),
                Stream::Err => (&err_buffer, &mut err_open),
            };
            match result {
                Ok(0) | Err(_) => *open = false,
                Ok(count) => kept.push(buffer.get(..count).unwrap_or_default()),
            }
        }
        // The output is closed, but the command runs until its leader
        // exits, and the cancel and the timeout still stop it.
        let why = if let Some(why) = stopped {
            why
        } else {
            let ended = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(Stopped::Cancelled),
                () = &mut timeout => Err(Stopped::TimedOut),
                status = group.wait() => Ok(status?),
            };
            match ended {
                Ok(status) => return Ok((exit_status(status), kept)),
                Err(why) => {
                    group.kill();
                    why
                }
            }
        };
        // Reaped within the drain, or left to `Group`'s drop (one more kill,
        // best effort): a leader the kill could not reach must not hang the
        // call, and the stopped message already says it may still run.
        tokio::select! {
            biased;
            _reaped = group.wait() => {}
            () = self.clock.sleep(options.drain) => {}
        }
        Ok((ExitStatus::Stopped(why), kept))
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::{ExitStatus, exit_status};

    #[test]
    fn a_command_nobody_stopped_reports_its_code_or_signal() {
        // Raw wait statuses: `code << 8` for an exit, the signal number for
        // a death by signal.
        let term = rustix::process::Signal::TERM.as_raw();
        assert_eq!(
            exit_status(std::process::ExitStatus::from_raw(3 << 8)),
            ExitStatus::Code(3)
        );
        assert_eq!(
            exit_status(std::process::ExitStatus::from_raw(term)),
            ExitStatus::Signal(term)
        );
    }
}
