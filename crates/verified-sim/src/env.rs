//! The simulated environment: real file tools on turmoil's filesystem, a fake
//! shell (turmoil has no processes), and every mutation written to the
//! world [`Ledger`], which survives crashes.
//!
//! The fake shell understands:
//!
//! - `touch NAME`: a side effect, nothing else
//! - `sleep MS`: waits on the Clock seam until done, cancelled or timed out
//! - `false`: exits 1
//! - `hang`: never returns, whatever it is told: a run that ignores its
//!   cancel and its kill, which only v's deadline ends (P1)
//! - anything else: echoes the command, exits 0
//!
//! An `edit`'s write is ledgered with the text it writes when it starts, and
//! again when it returns: the checks judge each edit by what it wrote, and
//! the file after the final crash by the writes that finished.

use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use verified_domain::{ActorId, EffectId, Millis};
use verified_server::env::{
    BashGrant, DirEntry, EditGrant, Env, EnvError, ExecOptions, ExitStatus, Kept, LocalEnv,
    Stopped, Written,
};
use verified_server::payload::ProjectPath;
use verified_server::seam::Clock;

use crate::ledger::{Entry, Ledger, Op};

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

    fn mutated(&self, actor: ActorId, call: Option<String>, effect: EffectId, op: Op) {
        self.ledger.record(Entry::Mutation {
            actor,
            call,
            effect,
            op,
        });
    }
}

#[async_trait]
impl Env for SimEnv {
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

    /// Ledgered with the text it writes, then written; ledgered again once
    /// the write returns.
    async fn write(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<Written, EnvError> {
        self.mutated(
            edit.actor(),
            Some(edit.id().as_str().to_owned()),
            edit.effect(),
            Op::Write {
                path: edit.path().as_str().to_owned(),
                text: String::from_utf8_lossy(contents).into_owned(),
            },
        );
        let written = self.files.write(edit, contents).await?;
        self.ledger.record(Entry::Wrote {
            actor: edit.actor(),
            effect: edit.effect(),
        });
        Ok(written)
    }

    async fn rules_file(&self, limit: u64) -> Result<Option<Vec<u8>>, EnvError> {
        self.files.rules_file(limit).await
    }

    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        cancel: &CancellationToken,
    ) -> Result<(ExitStatus, Kept), EnvError> {
        let script = bash.command().as_str().trim();
        self.mutated(
            bash.actor(),
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
                        (ExitStatus::Stopped(Stopped::TimedOut), String::new())
                    } else {
                        (ExitStatus::Code(0), format!("slept {millis}ms\n"))
                    }
                }
                () = cancel.cancelled() => (ExitStatus::Stopped(Stopped::Cancelled), String::new()),
            }
        } else if script == "hang" {
            std::future::pending::<()>().await;
            (ExitStatus::Code(0), String::new())
        } else if script == "false" {
            (ExitStatus::Code(1), String::new())
        } else {
            (ExitStatus::Code(0), format!("{script}\n"))
        };
        let mut kept = Kept::new(options.output_bytes);
        kept.push(said.as_bytes());
        Ok((status, kept))
    }
}
