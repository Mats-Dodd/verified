//! The simulated environment: real file tools on turmoil's filesystem, a fake
//! shell (turmoil has no processes), and every mutation written to the
//! world [`Ledger`], which survives crashes.
//!
//! The fake shell understands:
//!
//! - `touch NAME`: a side effect, nothing else
//! - `sleep MS`: waits on the Clock seam until done, cancelled or timed out
//! - `false`: exits 1
//! - anything else: echoes the command, exits 0
//!
//! `edit`'s marker ([`Env::mark`]) is the daemon's own bookkeeping, not a
//! change to the project, so it is no ledger mutation; but once it is durable
//! the env stops at [`Point::EditMarked`], a barrier the harness may crash
//! the daemon at, between the marker and the rename.

use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::id::SessionId;
use verified_domain::{EffectId, Millis};
use verified_server::env::{
    BashGrant, DirEntry, EditGrant, Env, EnvError, ExecOptions, ExitStatus, Fingerprint, LocalEnv,
    MarkerKey, Stat, Stopped,
};
use verified_server::payload::ProjectPath;
use verified_server::seam::Clock;

use crate::ledger::{Entry, Ledger, Op};

/// Points in the simulated environment a crash may be aimed at
/// (`turmoil::barriers`), next to the daemon's own `shim::Barrier`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// An `edit`'s marker is durable; its staged file is not renamed yet.
    EditMarked,
}

/// File tools on the simulated disk, a fake shell, and the ledger.
#[derive(Clone)]
pub struct SimEnv {
    files: LocalEnv,
    clock: Arc<dyn Clock>,
    ledger: Ledger,
}

impl fmt::Debug for SimEnv {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SimEnv").finish_non_exhaustive()
    }
}

impl SimEnv {
    #[must_use]
    pub const fn new(files: LocalEnv, clock: Arc<dyn Clock>, ledger: Ledger) -> Self {
        Self {
            files,
            clock,
            ledger,
        }
    }

    fn mutated(&self, call: Option<String>, effect: EffectId, op: Op) {
        self.ledger.record(Entry::Mutation { call, effect, op });
    }
}

#[async_trait]
impl Env for SimEnv {
    async fn stat(&self, path: &ProjectPath) -> Result<Stat, EnvError> {
        self.files.stat(path).await
    }

    async fn read(&self, path: &ProjectPath, limit: u64) -> Result<Vec<u8>, EnvError> {
        self.files.read(path, limit).await
    }

    async fn list(
        &self,
        path: Option<&ProjectPath>,
        limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError> {
        self.files.list(path, limit).await
    }

    async fn stage(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<(), EnvError> {
        self.mutated(
            Some(edit.id().as_str().to_owned()),
            edit.effect(),
            Op::Write {
                path: edit.staged().as_str().to_owned(),
            },
        );
        self.files.stage(edit, contents).await
    }

    async fn marked(&self, edit: &EditGrant<'_>) -> Result<Option<Fingerprint>, EnvError> {
        self.files.marked(edit).await
    }

    /// The daemon's own bookkeeping (under `.verified/`), not a change to
    /// the project: not a ledger mutation. Durable, it is a barrier.
    async fn mark(&self, edit: &EditGrant<'_>, original: Fingerprint) -> Result<(), EnvError> {
        self.files.mark(edit, original).await?;
        turmoil::barriers::trigger(Point::EditMarked).await;
        Ok(())
    }

    async fn commit(&self, edit: &EditGrant<'_>) -> Result<(), EnvError> {
        self.mutated(
            Some(edit.id().as_str().to_owned()),
            edit.effect(),
            Op::Rename {
                from: edit.staged().as_str().to_owned(),
                to: edit.path().as_str().to_owned(),
            },
        );
        self.files.commit(edit).await
    }

    async fn discard(&self, edit: &EditGrant<'_>) -> Result<(), EnvError> {
        self.mutated(
            Some(edit.id().as_str().to_owned()),
            edit.effect(),
            Op::Remove {
                path: edit.staged().as_str().to_owned(),
            },
        );
        self.files.discard(edit).await
    }

    async fn markers(&self, session: SessionId, limit: usize) -> Result<Vec<MarkerKey>, EnvError> {
        self.files.markers(session, limit).await
    }

    /// Cleanup of a resolved call's bookkeeping (its marker, and a staged
    /// copy nothing will rename): no change the user's files see.
    async fn unmark(&self, key: MarkerKey, staged: &ProjectPath) -> Result<(), EnvError> {
        self.files.unmark(key, staged).await
    }

    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        output: mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<ExitStatus, EnvError> {
        let script = bash.command().as_str().trim();
        self.mutated(
            Some(bash.id().as_str().to_owned()),
            bash.effect(),
            Op::Exec {
                command: script.to_owned(),
            },
        );
        let (status, said) = if let Some(name) = script.strip_prefix("touch ") {
            (ExitStatus::Code(0), format!("touched {name}\n"))
        } else if let Some(millis) = script.strip_prefix("sleep ") {
            let millis = millis.trim().parse::<u64>().unwrap_or(0);
            let wait = Millis::new(millis.min(options.timeout.get()));
            tokio::select! {
                biased;
                () = self.clock.sleep(wait) => {
                    if millis > options.timeout.get() {
                        (ExitStatus::Killed(Stopped::TimedOut), String::new())
                    } else {
                        (ExitStatus::Code(0), format!("slept {millis}ms\n"))
                    }
                }
                () = cancel.cancelled() => (ExitStatus::Killed(Stopped::Cancelled), String::new()),
            }
        } else if script == "false" {
            (ExitStatus::Code(1), String::new())
        } else {
            (ExitStatus::Code(0), format!("{script}\n"))
        };
        if !said.is_empty() {
            // The collector stops reading only when the tool gives up; output
            // nobody reads is output nobody needs.
            drop(output.send(said.into_bytes()).await);
        }
        Ok(status)
    }
}
