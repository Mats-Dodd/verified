//! Seams for time, randomness and the daemon lock, with their real impls,
//! and the world layer: the only place the daemon starts a task.
//!
//! The Model, Log and Env seams live with the code that uses them
//! ([`crate::model`], [`crate::log`], [`crate::env`]).

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use verified_domain::Millis;
use verified_domain::id::{IdEntropy, UnixMillis};

/// Wall time and sleeping.
#[async_trait]
pub trait Clock: Send + Sync + fmt::Debug {
    /// The wall time, for ids and stream cursors.
    fn now(&self) -> UnixMillis;

    /// Waits `delay`. Cancel-safe: dropping the future just stops waiting.
    async fn sleep(&self, delay: Millis);
}

/// Randomness for ids and jitter.
pub trait Entropy: Send + Sync + fmt::Debug {
    /// Fills `out` with random bytes.
    fn fill(&self, out: &mut [u8]);
}

/// The 10 random bytes of a `UUIDv7`.
#[must_use]
pub fn id_entropy(entropy: &dyn Entropy) -> IdEntropy {
    let mut bytes = IdEntropy::default();
    entropy.fill(&mut bytes);
    bytes
}

/// A random `u64`.
#[must_use]
pub fn entropy_u64(entropy: &dyn Entropy) -> u64 {
    let mut bytes = [0u8; 8];
    entropy.fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// The OS clock and tokio's timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    #[expect(
        clippy::disallowed_methods,
        reason = "the Clock seam's real impl is where wall time enters"
    )]
    fn now(&self) -> UnixMillis {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock is set after 1970");
        let millis =
            u64::try_from(since.as_millis()).expect("the system clock is set before year 584556");
        UnixMillis::new(millis).expect("the system clock is set before year 10889")
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the Clock seam's real impl is where sleeping enters"
    )]
    async fn sleep(&self, delay: Millis) {
        tokio::time::sleep(core::time::Duration::from_millis(delay.get())).await;
    }
}

/// The OS entropy source.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    #[expect(
        clippy::disallowed_methods,
        reason = "the Entropy seam's real impl is where randomness enters"
    )]
    fn fill(&self, out: &mut [u8]) {
        getrandom::fill(out).expect("the OS entropy source is available");
    }
}

/// Why the daemon lock was not taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another daemon holds {path}")]
    Held { path: String },
    #[error("cannot open the lock file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// A held lock; dropping it (or the process dying) releases it.
#[derive(Debug)]
pub struct LockGuard {
    /// The flocked file, for the real lock (closing it releases the lock);
    /// nothing for a simulated one.
    _file: Option<std::fs::File>,
}

impl LockGuard {
    /// A guard that holds nothing, for locks that need no handle.
    #[must_use]
    pub const fn unheld() -> Self {
        Self { _file: None }
    }
}

/// One daemon per project: an exclusive lock on a file. Its own seam
/// because turmoil's filesystem has no locking.
#[async_trait]
pub trait Lock: Send + Sync + fmt::Debug {
    /// Takes the lock at `path` without waiting.
    ///
    /// # Errors
    /// [`LockError::Held`] when another process holds it.
    async fn lock(&self, path: &Path) -> Result<LockGuard, LockError>;
}

/// `flock(LOCK_EX | LOCK_NB)` on the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileLock;

#[async_trait]
impl Lock for FileLock {
    #[expect(
        clippy::disallowed_methods,
        reason = "the Lock seam's real impl: locking is outside the fs shim, which turmoil cannot lock"
    )]
    async fn lock(&self, path: &Path) -> Result<LockGuard, LockError> {
        let shown = path.display().to_string();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| LockError::Io {
                path: shown.clone(),
                source,
            })?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(LockGuard { _file: Some(file) }),
            Err(errno) if errno == rustix::io::Errno::WOULDBLOCK => {
                Err(LockError::Held { path: shown })
            }
            Err(errno) => Err(LockError::Io {
                path: shown,
                source: errno.into(),
            }),
        }
    }
}

/// Grants every lock: for a simulation, where each host runs one daemon.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLock;

#[async_trait]
impl Lock for NoLock {
    async fn lock(&self, _path: &Path) -> Result<LockGuard, LockError> {
        Ok(LockGuard::unheld())
    }
}

/// The world layer: the only place the daemon starts tasks. Every task
/// belongs to one daemon; `shutdown` tells them to stop at a safe point.
///
/// Crash-only (Law 8): a task that panics takes the daemon down, and
/// recovery on restart is the way back. Every task is supervised: a panic
/// is caught where the task is polled, fires [`World::crashed`], and is
/// handed to the crash shim ([`crate::shim::crash`]). In the shipped daemon
/// that aborts the process on the spot, so no other task runs another step
/// (no log append, no tool) after a broken invariant. Under the `sim`
/// feature a process abort would kill the whole test run, so the shim
/// returns instead and every task stops at its next safe point;
/// [`crate::daemon::serve`] then returns [`crate::daemon::DaemonError::Panicked`],
/// the turmoil host's software fails, and the simulation reports the run as
/// failed.
#[derive(Debug, Clone, Default)]
pub struct World {
    shutdown: CancellationToken,
    crashed: CancellationToken,
    groups: ProcessGroups,
}

/// How often [`ProcessGroups::kill_all`] tries the set's lock, yielding
/// between tries, before it gives up.
const KILL_ATTEMPTS: u32 = 10_000;

/// The process groups of the commands running now.
///
/// On a panic ([`ProcessGroups::kill_on_panic`]) or a fatal error (as each
/// command's future is dropped) the daemon kills them: `kill_on_drop` alone
/// reaches only a group's leader, never the grandchildren a command left
/// running. A signal that kills the daemon bypasses both.
#[derive(Debug, Clone, Default)]
pub struct ProcessGroups {
    #[expect(
        clippy::disallowed_types,
        reason = "writers: the Env seam's exec, as a group starts and once it is reaped or dropped; \
                  reader: the panic hook, before the process aborts; invariant: holds only \
                  groups whose leader is not yet reaped (so no id is reused); dropped with the world"
    )]
    live: Arc<std::sync::Mutex<BTreeSet<i32>>>,
}

impl ProcessGroups {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<i32>> {
        // A panic while holding it left a set that is still accurate: the
        // commands carry on being registered and unregistered.
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records that process group `group` is running.
    pub fn insert(&self, group: i32) {
        self.lock().insert(group);
    }

    /// Records that process group `group` was reaped or killed.
    pub fn remove(&self, group: i32) {
        self.lock().remove(&group);
    }

    /// How many groups are running.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no group is running.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Kills every running group on any panic, before the process aborts
    /// (`panic = "abort"` drops nothing and runs no `catch_unwind`) or
    /// unwinds: chains [`ProcessGroups::kill_all`] after the current panic
    /// hook.
    pub fn kill_on_panic(&self) {
        let groups = self.clone();
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |panicked| {
            previous(panicked);
            groups.kill_all();
        }));
    }

    /// Sends `SIGKILL` to every running group. Best effort: the daemon is
    /// going down, and a group that is gone already is what it wants. It
    /// never blocks on the set's lock, since the panic hook may run on the
    /// thread holding it (a panic inside `insert` or `remove`) and would
    /// then never reach the abort: it tries a bounded number of times, which
    /// outlasts another thread's insert or remove, and only then kills
    /// nothing. A poisoned set is still accurate (see `lock`).
    pub fn kill_all(&self) {
        let mut attempts = 0..KILL_ATTEMPTS;
        let live = loop {
            match self.live.try_lock() {
                Ok(live) => break live,
                Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if attempts.next().is_none() {
                        return;
                    }
                    std::thread::yield_now();
                }
            }
        };
        for group in live.iter().copied() {
            if let Some(pid) = rustix::process::Pid::from_raw(group) {
                let _best_effort: Result<(), rustix::io::Errno> =
                    rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

impl World {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `task` concurrently. The task owns its own completion: it reports
    /// through a channel and is never cancelled by dropping it. A panic in
    /// it crashes the daemon (see [`World`]).
    #[expect(
        clippy::disallowed_methods,
        reason = "the world layer is the one place the daemon starts a task; each is supervised"
    )]
    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let supervised = Supervised {
            task: Box::pin(task),
            world: self.clone(),
        };
        drop(tokio::spawn(supervised));
    }

    /// The process groups of the commands running now.
    #[must_use]
    pub const fn groups(&self) -> &ProcessGroups {
        &self.groups
    }

    /// Fires when the daemon is stopping.
    #[must_use]
    pub const fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Fires when a task panicked (seen only under the `sim` feature: the
    /// shipped daemon aborts at once).
    #[must_use]
    pub const fn crashed(&self) -> &CancellationToken {
        &self.crashed
    }

    /// Records that a task panicked and stops every task at its next safe
    /// point.
    pub(crate) fn crash(&self) {
        self.crashed.cancel();
        self.shutdown.cancel();
    }
}

/// A task under the world's supervision: a panic while polling it is the
/// daemon's crash, never a silently finished task.
struct Supervised {
    task: Pin<Box<dyn Future<Output = ()> + Send>>,
    world: World,
}

impl Future for Supervised {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let task = &mut this.task;
        // The task is never polled again after a panic: it is finished, and
        // its state is dropped with this future.
        match panic::catch_unwind(AssertUnwindSafe(|| task.as_mut().poll(cx))) {
            Ok(poll) => poll,
            Err(_payload) => {
                this.world.crash();
                crate::shim::crash::task_panicked();
                Poll::Ready(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

    use super::ProcessGroups;

    /// A panic that poisoned the set leaves it accurate, so a later panic's
    /// hook still kills every running group.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts its own command, as the Env seam does"
    )]
    fn a_poisoned_set_still_kills() {
        let groups = ProcessGroups::default();
        let mut command = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .expect("a command");
        groups.insert(i32::try_from(command.id()).expect("a pid"));
        let poisoned = std::panic::catch_unwind(|| {
            let _held = groups.lock();
            #[expect(clippy::panic, reason = "the test's point: poison the set")]
            {
                panic!("a panic while the set is held");
            }
        });
        assert!(poisoned.is_err() && groups.live.is_poisoned(), "poisoned");
        groups.kill_all();
        let status = command.wait().expect("the command ends");
        assert_eq!(
            status.signal(),
            Some(rustix::process::Signal::KILL.as_raw()),
            "killed: {status:?}"
        );
    }
}
