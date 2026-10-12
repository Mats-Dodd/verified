//! The `Env` seam: the project's files and a shell.
//!
//! Shaped after pi's `ExecutionEnv`
//! (`packages/agent/src/harness/types.ts`). Every method
//! returns a `Result` and never panics; paths are [`ProjectPath`]s, parsed
//! once at the boundary. Mutating methods take a grant ([`EditGrant`],
//! [`BashGrant`]) that only [`Grant::of`] makes, from a core-minted
//! `ToolCall<Approved, _>`, and everything a mutation may touch (the file, its
//! staged sibling, the command) is read from that call. So running a
//! mutation without a durable approval does not compile, and an approval
//! for one call cannot authorize another call's mutation.

use core::fmt;
use core::pin::Pin;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_core::{Approved, ToolCall};
use verified_domain::id::{CallId, SessionId};
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
    #[error("{path}: is a symlink; links are not followed")]
    Symlink { path: String },
    #[error("the shell could not start: {reason}")]
    Spawn { reason: String },
    /// A command's process group could not be killed: it may still run.
    #[error(transparent)]
    Kill(#[from] KillError),
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

/// What is at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    Missing,
    File { bytes: u64 },
    Dir,
    Symlink,
    Other,
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
    Code(i32),
    /// Killed by a signal it was not sent by us.
    Signal,
    /// We killed its process group.
    Killed(Stopped),
    /// The group was stopped, but something outside it (a process that
    /// left the group, e.g. with `setsid`) still held the output open past
    /// the drain: it may still be running.
    Escaped(Stopped),
}

/// How to run a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOptions {
    pub timeout: Millis,
    /// See `ServerLimits::exec_reap_schedule`.
    pub reap_schedule: Vec<Millis>,
    /// How long output is still read once the group is stopped.
    pub drain: Millis,
    /// Extra environment variables (e.g. `VERIFIED_CALL_ID`).
    pub env: Vec<(String, String)>,
}

/// What an approved call may do, derived from the call alone.
#[derive(Debug)]
pub enum Grant<'call> {
    /// Read a file or list a directory: no mutation.
    Read(&'call ProjectPath),
    Edit(EditGrant<'call>),
    Bash(BashGrant<'call>),
}

/// A fingerprint of a file's bytes: their length and 64-bit FNV-1a hash.
///
/// An edit's marker records the fingerprint of the text it was computed
/// from, so a rerun renames its staged copy only over that same text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    bytes: u64,
    hash: u64,
}

impl Fingerprint {
    /// The fingerprint of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            hash,
        }
    }

    /// As a marker stores it: `<length>:<hash in hex>`.
    #[must_use]
    pub fn to_text(self) -> String {
        format!("{}:{:016x}", self.bytes, self.hash)
    }

    /// The fingerprint in a marker's `text`, if it holds one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (bytes, hash) = text.trim().split_once(':')?;
        if hash.len() != 16 {
            return None;
        }
        Some(Self {
            bytes: bytes.parse().ok()?,
            hash: u64::from_str_radix(hash, 16).ok()?,
        })
    }
}

/// The key of an edit's marker: the session and its `Dispatched` seq.
///
/// The seq of the call's `Dispatched` record. Unique for good: a model's tool call ids are unique
/// only within one response, but a seq names one record of one log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarkerKey {
    pub session: SessionId,
    pub seq: u64,
}

impl MarkerKey {
    /// The key in a marker's file name, if it is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        // A session id is 36 characters; the seq follows a `-`.
        let session = SessionId::parse(name.get(..36)?).ok()?;
        let seq = name.get(36..)?.strip_prefix('-')?;
        if seq.is_empty() || !seq.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            session,
            seq: seq.parse().ok()?,
        })
    }
}

impl fmt::Display for MarkerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.session, self.seq)
    }
}

/// An approved `edit`: replace `old` by `new` in `path`, by writing a staged
/// sibling named by the call's key and renaming it over `path`.
///
/// Rerun after a crash, the edit must know whether its rename happened, and
/// the file's text cannot tell (`new` may be empty, may contain `old`, or may
/// already appear elsewhere). So before the rename the edit makes a durable
/// marker named by its [`MarkerKey`]; see [`Env::mark`].
#[derive(Debug)]
pub struct EditGrant<'call> {
    id: &'call CallId,
    effect: EffectId,
    path: &'call ProjectPath,
    old: &'call str,
    new: &'call str,
    staged: ProjectPath,
    marker: MarkerKey,
}

/// An approved `bash`: run `command`.
#[derive(Debug, Clone, Copy)]
pub struct BashGrant<'call> {
    id: &'call CallId,
    effect: EffectId,
    command: &'call Command,
}

impl<'call> Grant<'call> {
    /// What `call`, of `session`, may do. The only way to make an
    /// [`EditGrant`] or a [`BashGrant`]. Never fails: every name it derives
    /// is valid by construction ([`ProjectPath::sibling`]).
    #[must_use]
    pub fn of(call: &'call ToolCall<Approved, Parsed>, session: SessionId) -> Self {
        let effect = call.effect();
        match call.args() {
            ToolArgs::Read { path } => Self::Read(path),
            ToolArgs::Edit { path, old, new } => Self::Edit(EditGrant {
                id: call.id(),
                effect,
                path,
                old,
                new,
                staged: path.sibling(&Sibling::staged(session, effect.seq)),
                marker: MarkerKey {
                    session,
                    seq: effect.seq,
                },
            }),
            ToolArgs::Bash { command } => Self::Bash(BashGrant {
                id: call.id(),
                effect,
                command,
            }),
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

    /// The sibling the new contents are staged in before the rename.
    #[must_use]
    pub const fn staged(&self) -> &ProjectPath {
        &self.staged
    }

    /// This edit's marker: its session and `Dispatched` seq, which together
    /// name one call for good.
    #[must_use]
    pub const fn marker(&self) -> MarkerKey {
        self.marker
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
    /// What is at `path`; symlinks are reported, not followed, and a path
    /// through a symlinked directory is refused.
    ///
    /// # Errors
    /// Any failure but not-found; a symlink on the way.
    async fn stat(&self, path: &ProjectPath) -> Result<Stat, EnvError>;

    /// The bytes of the file at `path`, at most `limit`.
    ///
    /// # Errors
    /// Missing, a directory, a symlink (at the path or on the way), larger
    /// than `limit`, or IO.
    async fn read(&self, path: &ProjectPath, limit: u64) -> Result<Vec<u8>, EnvError>;

    /// The entries of the directory at `path` (`None`: the project root),
    /// sorted, at most `limit`.
    ///
    /// # Errors
    /// Missing, not a directory, a symlink on the way, too many entries, or
    /// IO.
    async fn list(
        &self,
        path: Option<&ProjectPath>,
        limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError>;

    /// Creates or replaces the edit's staged file with `contents`, synced
    /// with its directory entry.
    ///
    /// # Errors
    /// A symlink on the way, or IO.
    async fn stage(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<(), EnvError>;

    /// The edit's marker, if it got as far as its rename: the fingerprint
    /// of the text its staged copy was made from. A marker that does not
    /// parse was torn by a crash while it was being made, before any
    /// rename: that is no marker.
    ///
    /// # Errors
    /// IO.
    async fn marked(&self, edit: &EditGrant<'_>) -> Result<Option<Fingerprint>, EnvError>;

    /// Makes the edit's marker durable (file and directory synced), with
    /// the fingerprint of the text the staged copy was made from. An edit
    /// marks itself after staging and before renaming, so on a rerun a
    /// marker with the staged file still there means "rename it, if the
    /// file still holds that text", and a marker without it means "already
    /// applied". Markers live in the daemon's directory, never among the
    /// project's files.
    ///
    /// # Errors
    /// IO.
    async fn mark(&self, edit: &EditGrant<'_>, original: Fingerprint) -> Result<(), EnvError>;

    /// Renames the edit's staged file onto its path atomically and durably
    /// (the directory is synced).
    ///
    /// # Errors
    /// A symlink on the way, or IO.
    async fn commit(&self, edit: &EditGrant<'_>) -> Result<(), EnvError>;

    /// Removes the edit's staged file, if any: an edit that failed after
    /// staging leaves nothing behind.
    ///
    /// # Errors
    /// IO.
    async fn discard(&self, edit: &EditGrant<'_>) -> Result<(), EnvError>;

    /// The markers `session` left, at most `limit`.
    ///
    /// # Errors
    /// More than `limit`, or IO.
    async fn markers(&self, session: SessionId, limit: usize) -> Result<Vec<MarkerKey>, EnvError>;

    /// Removes what an edit left for a rerun: its staged copy `staged` (if
    /// any), then its marker. Only for a call whose result is durable: it is
    /// never rerun, so neither is read again. Removing what is missing
    /// succeeds, and a crash between the two leaves a marker without its
    /// copy, which the next sweep removes.
    ///
    /// # Errors
    /// IO.
    async fn unmark(&self, key: MarkerKey, staged: &ProjectPath) -> Result<(), EnvError>;

    /// Runs the approved command with `sh -c` in the project root, in its
    /// own process group, streaming combined output into `output` (dropped
    /// on return). `cancel` and the timeout kill the whole group; the call
    /// still returns what happened. Once the group is stopped, its output is
    /// read for at most `ExecOptions::drain` more: a process that left the
    /// group and holds the output open makes the result
    /// [`ExitStatus::Escaped`], never a hang.
    ///
    /// # Errors
    /// The shell could not start, or its group could not be killed.
    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        output: mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<ExitStatus, EnvError>;
}

/// The real environment: the project directory through the fs shim, and
/// `/bin/sh` through tokio's process support.
///
/// Symlinks are never followed: every component of a path is checked with
/// `lstat` before the path is used, so neither an auto-approved `read` nor an
/// approved `edit` can reach outside the project through a link. A link
/// swapped in between the check and the use (by a process racing the agent)
/// is not caught; v0's sandbox is the approval, not the filesystem.
#[derive(Debug, Clone)]
pub struct LocalEnv {
    root: PathBuf,
    clock: Arc<dyn Clock>,
    groups: ProcessGroups,
}

/// Where edit markers live, under the daemon's directory.
pub const MARKERS_DIR: &str = "edits";

impl LocalEnv {
    /// The project at `root`; commands it starts are registered in `groups`
    /// (the world's, so a crashing daemon kills them).
    #[must_use]
    pub const fn new(root: PathBuf, clock: Arc<dyn Clock>, groups: ProcessGroups) -> Self {
        Self {
            root,
            clock,
            groups,
        }
    }

    fn markers_dir(&self) -> PathBuf {
        self.root.join(crate::daemon::DIR).join(MARKERS_DIR)
    }

    /// `path` under the root, once every directory on the way (and, if
    /// `whole`, the path itself) is checked not to be a symlink. A missing
    /// component ends the check: nothing below it exists to be a link.
    async fn resolve(&self, path: &ProjectPath, whole: bool) -> Result<PathBuf, EnvError> {
        let components: Vec<&str> = path.as_str().split('/').collect();
        let checked = if whole {
            components.len()
        } else {
            components.len().saturating_sub(1)
        };
        let mut at = self.root.clone();
        let mut shown = String::new();
        for component in components.iter().take(checked) {
            at.push(component);
            if !shown.is_empty() {
                shown.push('/');
            }
            shown.push_str(component);
            match fs::stat(&at)
                .await
                .map_err(|error| EnvError::from_io(&shown, &error))?
            {
                None => break,
                Some((fs::Kind::Symlink, _)) => return Err(EnvError::Symlink { path: shown }),
                Some((fs::Kind::File | fs::Kind::Dir | fs::Kind::Other, _)) => {}
            }
        }
        Ok(path.under(&self.root))
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

/// Why a command's process group could not be killed. Only "no such
/// group" (ESRCH, it already ended) is expected; anything else means it may
/// still be running.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot kill the process group of {pid}: {reason}")]
pub struct KillError {
    pub pid: String,
    pub reason: String,
}

/// What a kill of a process group did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    /// `SIGKILL` was delivered to the group.
    Sent,
    /// The group had already ended (ESRCH): nothing was killed.
    Gone,
}

/// Sends `SIGKILL` to the process group `group`; ESRCH is [`Kill::Gone`],
/// any other failure is returned as is.
fn kill_group(group: rustix::process::Pid) -> Result<Kill, rustix::io::Errno> {
    match rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
        Ok(()) => Ok(Kill::Sent),
        Err(errno) if errno == rustix::io::Errno::SRCH => Ok(Kill::Gone),
        Err(errno) => Err(errno),
    }
}

/// A running command's process group: the child that leads it, and its
/// exit status once reaped.
///
/// Registered in the world's [`ProcessGroups`] while its leader is not yet
/// reaped. Dropped before that (the daemon stopping with the command still
/// running), it kills the whole group: `kill_on_drop` alone signals only
/// the leader, and the command's other processes would outlive the daemon.
/// Once the leader is reaped the group id may be reused by an unrelated
/// process, so nothing signals it again.
struct Group {
    child: tokio::process::Child,
    /// The group id (the leader's pid), fixed at spawn.
    id: Option<rustix::process::Pid>,
    reaped: Option<std::process::ExitStatus>,
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
            reaped: None,
            groups,
        }
    }

    fn error(&self, reason: String) -> KillError {
        KillError {
            pid: self
                .id
                .map_or_else(|| "?".to_owned(), |id| id.as_raw_nonzero().to_string()),
            reason,
        }
    }

    /// Records the leader's exit status, after which the group id is never
    /// signalled again.
    fn reap(&mut self, status: std::process::ExitStatus) {
        self.reaped = Some(status);
        if let Some(id) = self.id {
            self.groups.remove(id.as_raw_nonzero().get());
        }
    }

    /// Sends `SIGKILL` to the whole group.
    ///
    /// ESRCH (the group is gone) is the expected failure. One more is
    /// expected on macOS: a group whose members are all zombies (the leader
    /// exited, not yet reaped) answers EPERM, since nothing in it can be
    /// signalled. So an EPERM reaps the leader if it has exited and asks
    /// once more: with the leader reaped, an empty group answers ESRCH. Any
    /// other answer, or an EPERM while the leader still runs, is an error:
    /// the group may still be running. While any member lives the group id
    /// cannot be reused, so the retry cannot reach another group.
    fn kill(&mut self) -> Result<Kill, KillError> {
        let Some(id) = self.id else {
            return Err(self.error("no process id".to_owned()));
        };
        if self.reaped.is_some() {
            // The leader is reaped: the id may name another group by now.
            return Ok(Kill::Gone);
        }
        match kill_group(id) {
            Ok(kill) => Ok(kill),
            Err(errno) if errno == rustix::io::Errno::PERM => {
                let exited = self
                    .child
                    .try_wait()
                    .map_err(|error| self.error(error.to_string()))?;
                let Some(status) = exited else {
                    return Err(self.error(errno.to_string()));
                };
                // Ask once more while the id is still ours (the zombie
                // leader holds it), then reap.
                let again = kill_group(id).map_err(|again| self.error(again.to_string()));
                self.reap(status);
                again
            }
            Err(errno) => Err(self.error(errno.to_string())),
        }
    }

    /// Stops the group for `why`.
    fn stop(&mut self, why: Stopped) -> Result<Stop, KillError> {
        Ok(match self.kill()? {
            Kill::Sent => Stop::Killed(why),
            Kill::Gone => Stop::Ended(why),
        })
    }

    /// Kills strays, then reaps the leader (unless already reaped).
    ///
    /// A group that answers EPERM holds only members we cannot signal:
    /// zombies that init has not reaped yet, a process of another user, or
    /// a leader still exiting (macOS: no longer signallable, not yet
    /// waitable, so [`Group::kill`] cannot reap it yet). Each wait of
    /// `schedule` gives them time; once it is spent the group may still be
    /// running, which is an error.
    async fn finish(
        &mut self,
        clock: &dyn Clock,
        schedule: &[Millis],
    ) -> Result<std::process::ExitStatus, EnvError> {
        let mut waits = schedule.iter();
        loop {
            match self.kill() {
                Ok(Kill::Sent | Kill::Gone) => break,
                Err(error) => match waits.next() {
                    Some(wait) => clock.sleep(*wait).await,
                    None => return Err(error.into()),
                },
            }
        }
        if let Some(status) = self.reaped {
            return Ok(status);
        }
        let status = self.child.wait().await.map_err(|error| EnvError::Spawn {
            reason: error.to_string(),
        })?;
        self.reap(status);
        Ok(status)
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.reaped.is_none()
            && let Some(id) = self.id
        {
            // Best effort: the daemon is stopping, or the command's result
            // is already lost; either way nothing of it may keep running.
            let _best_effort: Result<Kill, rustix::io::Errno> = kill_group(id);
            self.groups.remove(id.as_raw_nonzero().get());
        }
    }
}

/// Where stopping a running command stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Nobody asked it to stop.
    Running,
    /// Asked to stop for this reason, and its group was sent `SIGKILL`.
    Killed(Stopped),
    /// Asked to stop for this reason, but its group had already ended: it
    /// was not killed.
    Ended(Stopped),
}

/// How a command ended: killed by us only if we sent the `SIGKILL` that
/// ended it, otherwise its own exit code or signal.
fn exit_status(stop: Stop, status: std::process::ExitStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    match (stop, status.code(), status.signal()) {
        (Stop::Running | Stop::Killed(_) | Stop::Ended(_), Some(code), _) => ExitStatus::Code(code),
        (Stop::Killed(why), None, Some(signal))
            if signal == rustix::process::Signal::KILL.as_raw() =>
        {
            ExitStatus::Killed(why)
        }
        (Stop::Running | Stop::Killed(_) | Stop::Ended(_), None, _) => ExitStatus::Signal,
    }
}

type Waiting<'wait> = Pin<Box<dyn Future<Output = ()> + Send + 'wait>>;

#[async_trait]
impl Env for LocalEnv {
    async fn stat(&self, path: &ProjectPath) -> Result<Stat, EnvError> {
        let at = self.resolve(path, false).await?;
        let found = fs::stat(&at)
            .await
            .map_err(|error| EnvError::from_io(path.as_str(), &error))?;
        Ok(match found {
            None => Stat::Missing,
            Some((fs::Kind::File, bytes)) => Stat::File { bytes },
            Some((fs::Kind::Dir, _)) => Stat::Dir,
            Some((fs::Kind::Symlink, _)) => Stat::Symlink,
            Some((fs::Kind::Other, _)) => Stat::Other,
        })
    }

    async fn read(&self, path: &ProjectPath, limit: u64) -> Result<Vec<u8>, EnvError> {
        let shown = || path.as_str().to_owned();
        match self.stat(path).await? {
            Stat::Missing => return Err(EnvError::NotFound { path: shown() }),
            Stat::Dir => return Err(EnvError::IsDirectory { path: shown() }),
            Stat::Symlink => return Err(EnvError::Symlink { path: shown() }),
            Stat::Other => {
                return Err(EnvError::Io {
                    path: shown(),
                    reason: "not a regular file".to_owned(),
                });
            }
            Stat::File { bytes } if bytes > limit => {
                return Err(EnvError::TooLarge {
                    path: shown(),
                    length: bytes,
                    limit,
                });
            }
            Stat::File { .. } => {}
        }
        let bytes = fs::read(&path.under(&self.root))
            .await
            .map_err(|error| EnvError::from_io(path.as_str(), &error))?;
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if length > limit {
            return Err(EnvError::TooLarge {
                path: shown(),
                length,
                limit,
            });
        }
        Ok(bytes)
    }

    async fn list(
        &self,
        path: Option<&ProjectPath>,
        limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError> {
        let shown = path.map_or(".", ProjectPath::as_str);
        let dir = match path {
            Some(path) => self.resolve(path, true).await?,
            None => self.root.clone(),
        };
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

    async fn stage(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<(), EnvError> {
        let staged = edit.staged();
        let file = self.resolve(staged, false).await?;
        fs::write_synced(&file, contents)
            .await
            .map_err(|error| EnvError::from_io(staged.as_str(), &error))?;
        // The staged file's entry is durable before the marker is made: a
        // marker must never outlive the file it vouches for.
        let dir = file.parent().unwrap_or(&self.root);
        fs::sync_dir(dir)
            .await
            .map_err(|error| EnvError::from_io(staged.as_str(), &error))
    }

    async fn marked(&self, edit: &EditGrant<'_>) -> Result<Option<Fingerprint>, EnvError> {
        let marker = self.markers_dir().join(edit.marker().to_string());
        let shown = marker.display().to_string();
        if fs::stat(&marker)
            .await
            .map_err(|error| EnvError::from_io(&shown, &error))?
            .is_none()
        {
            return Ok(None);
        }
        let bytes = fs::read(&marker)
            .await
            .map_err(|error| EnvError::from_io(&shown, &error))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.lines().nth(1).and_then(Fingerprint::parse))
    }

    async fn mark(&self, edit: &EditGrant<'_>, original: Fingerprint) -> Result<(), EnvError> {
        let markers = self.markers_dir();
        let shown = |path: &std::path::Path| path.display().to_string();
        fs::create_dir_all(&markers)
            .await
            .map_err(|error| EnvError::from_io(&shown(&markers), &error))?;
        if let Some(parent) = markers.parent() {
            fs::sync_dir(parent)
                .await
                .map_err(|error| EnvError::from_io(&shown(parent), &error))?;
        }
        let marker = markers.join(edit.marker().to_string());
        let text = format!("{}\n{}\n", edit.id(), original.to_text());
        fs::write_synced(&marker, text.as_bytes())
            .await
            .map_err(|error| EnvError::from_io(&shown(&marker), &error))?;
        fs::sync_dir(&markers)
            .await
            .map_err(|error| EnvError::from_io(&shown(&markers), &error))
    }

    async fn commit(&self, edit: &EditGrant<'_>) -> Result<(), EnvError> {
        let to = edit.path();
        let target = self.resolve(to, false).await?;
        fs::rename(&edit.staged().under(&self.root), &target)
            .await
            .map_err(|error| EnvError::from_io(to.as_str(), &error))?;
        // The rename is durable only once its directory is: without this a
        // crash can undo an edit whose result the log already holds.
        let dir = target.parent().unwrap_or(&self.root);
        fs::sync_dir(dir)
            .await
            .map_err(|error| EnvError::from_io(to.as_str(), &error))
    }

    async fn discard(&self, edit: &EditGrant<'_>) -> Result<(), EnvError> {
        let staged = edit.staged();
        let file = self.resolve(staged, false).await?;
        match fs::remove_file(&file).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(EnvError::from_io(staged.as_str(), &error)),
        }
    }

    async fn markers(&self, session: SessionId, limit: usize) -> Result<Vec<MarkerKey>, EnvError> {
        let dir = self.markers_dir();
        let shown = dir.display().to_string();
        if fs::stat(&dir)
            .await
            .map_err(|error| EnvError::from_io(&shown, &error))?
            .is_none()
        {
            return Ok(Vec::new());
        }
        let entries = fs::list(&dir, limit)
            .await
            .map_err(|error| EnvError::from_io(&shown, &error))?;
        Ok(entries
            .iter()
            .filter_map(|entry| MarkerKey::parse(&entry.name.to_string_lossy()))
            .filter(|key| key.session == session)
            .collect())
    }

    async fn unmark(&self, key: MarkerKey, staged: &ProjectPath) -> Result<(), EnvError> {
        let file = self.resolve(staged, false).await?;
        match fs::remove_file(&file).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(EnvError::from_io(staged.as_str(), &error)),
        }
        let marker = self.markers_dir().join(key.to_string());
        match fs::remove_file(&marker).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(EnvError::from_io(&marker.display().to_string(), &error)),
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the Env seam's real impl is the one place a process starts; it owns the process group"
    )]
    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        output: mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<ExitStatus, EnvError> {
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
            group.kill()?;
            return Err(EnvError::Spawn {
                reason: "no pipes".to_owned(),
            });
        };
        let mut out_buffer = vec![0u8; 8192];
        let mut err_buffer = vec![0u8; 8192];
        let (mut out_open, mut err_open) = (true, true);
        let mut stop = Stop::Running;
        let mut timeout = self.clock.sleep(options.timeout);
        // Armed once the group is stopped: output is read for at most this
        // long more, so a process that left the group and holds the pipes
        // cannot hang the call.
        let mut drain: Waiting<'_> = Box::pin(core::future::pending());
        let mut escaped = false;
        // A kill that fails returns at once: the group may still run, so
        // what happened is unknown and nothing here claims it was killed.
        // The group's drop then kills it again, best effort.
        //
        // Stopping comes before reading: a command that writes without
        // pause would otherwise starve the cancel and the timeout.
        while out_open || err_open {
            let running = matches!(stop, Stop::Running);
            let read = tokio::select! {
                biased;
                () = &mut drain => {
                    escaped = true;
                    break;
                }
                () = cancel.cancelled(), if running => {
                    stop = group.stop(Stopped::Cancelled)?;
                    drain = self.clock.sleep(options.drain);
                    continue;
                }
                () = &mut timeout, if running => {
                    stop = group.stop(Stopped::TimedOut)?;
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
                Ok(count) => {
                    let chunk = buffer.get(..count).unwrap_or_default().to_vec();
                    // A gone collector only loses output, never the result.
                    drop(output.send(chunk).await);
                }
            }
        }
        // Stray background processes die with the call.
        let status = group
            .finish(self.clock.as_ref(), &options.reap_schedule)
            .await?;
        Ok(match (escaped, stop) {
            (true, Stop::Killed(why) | Stop::Ended(why)) => ExitStatus::Escaped(why),
            (true, Stop::Running) | (false, _) => exit_status(stop, status),
        })
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

    use super::{ExitStatus, Kill, Stop, Stopped, exit_status, kill_group};

    /// Raw wait statuses: `code << 8` for an exit, the signal number for a
    /// death by signal.
    fn exited(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn signalled(signal: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(signal)
    }

    #[test]
    fn killed_only_when_our_kill_ended_it() {
        let kill = rustix::process::Signal::KILL.as_raw();
        let term = rustix::process::Signal::TERM.as_raw();
        let cancelled = Stop::Killed(Stopped::Cancelled);
        assert_eq!(
            exit_status(cancelled, signalled(kill)),
            ExitStatus::Killed(Stopped::Cancelled),
            "our SIGKILL ended it"
        );
        assert_eq!(
            exit_status(cancelled, exited(0)),
            ExitStatus::Code(0),
            "it exited on its own before the kill landed"
        );
        assert_eq!(
            exit_status(Stop::Ended(Stopped::Cancelled), signalled(kill)),
            ExitStatus::Signal,
            "the group was gone when we tried: not our kill"
        );
        assert_eq!(
            exit_status(cancelled, signalled(term)),
            ExitStatus::Signal,
            "another signal ended it"
        );
        assert_eq!(exit_status(Stop::Running, exited(3)), ExitStatus::Code(3));
    }

    #[test]
    fn a_missing_group_is_gone_not_an_error() {
        // Process ids are below 2^22 on Linux and below 100_000 on macOS.
        let absent = rustix::process::Pid::from_raw(0x7fff_fff0).expect("a pid");
        assert_eq!(kill_group(absent), Ok(Kill::Gone));
    }
}
