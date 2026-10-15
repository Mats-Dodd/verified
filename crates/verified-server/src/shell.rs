//! The shell loop: inbox, decide, persist, apply, dispatch.
//!
//! One task per session owns its [`Core`], its [`Log`] and the projected
//! stream. It takes one input at a time from a bounded inbox:
//!
//! 1. `decide` the event (commands carry ids minted from the Clock and
//!    Entropy seams as event data);
//! 2. append the decision's records to the log as one group commit and wait
//!    for the fsync;
//! 3. `apply` each record (a refusal is a defect: crash) and project it;
//! 4. answer the command;
//! 5. start the decision's effects through the [`Tasks`], each of which
//!    reports back into the same inbox.
//!
//! Model parts are batched before they reach the core (group commit:
//! `Limits::parts_per_batch` parts, `ServerLimits::record_budget` bytes or
//! `Limits::batch_window`, on the Clock seam), and every batch reaches the
//! inbox before the call's `ModelDone`. A response whose single part, or
//! whose tool calls together, would not fit one log line fails the call
//! (`ModelFault::TooLarge`): no record the shell makes is one the log refuses.
//! Effects are cancelled with tokens at safe points, never by dropping them.
//!
//! Storage is the world. Should the log refuse an append (its file is
//! full), nothing was written and nothing is applied, and the session fails
//! stop (T1): it decides nothing more, starts no effect and acknowledges no
//! command, answering each one full. It keeps serving its stream, and runs
//! already started report into nothing. A restart replays the same log and
//! carries on until an append is refused again; a turn left running stays
//! as the log says. Nothing acknowledged is ever lost, nothing acts without
//! a durable record, and the daemon never crash-loops on it.
//!
//! Law 7, as clarified: no side effect is ever cancelled by dropping its
//! future. Tool runs and log appends run to completion (a tool checks its
//! token at safe points; `bash` has its process group killed) and report
//! what actually happened. Only an in-flight network exchange that carries
//! an idempotent id may be abandoned: the model client drops its connection
//! once its exchange ends ([`crate::model::anthropic`]), and a live stream
//! response is dropped when its reader goes away. Neither changes durable
//! state: model parts count only once the shell commits them.

use core::fmt;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use verified_core::{
    Approved, Core, Corrupt, Decision, Effect, Event, Loaded, ModelExit, Outcome,
    Rejection as CoreRejection, Summary, ToolCall, Verdict,
};
use verified_domain::id::{ActorId, CallId, MessageId, TurnId};
use verified_domain::{
    Address, Body, Born, EffectId, Envelope, EnvelopeId, Exit, Grant, Measured, Millis, ModelFault,
    Proposal, Record, Tool, TurnOutcome, Unstarted, named,
};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{
    ActorSummary, Approve, ApproveReply, Cancel, CancelReply, Compact, CompactReply,
    Outcome as WireOutcome, Rejection, SendMessage, SendMessageReply, StopActor, StopReply,
    Verdict as WireVerdict,
};
use verified_wire::stream::{Freshness, Offset, encode_chunks};

use crate::daemon::Registry;
use crate::defect;
use crate::env::Env;
use crate::history;
use crate::limits::ServerLimits;
use crate::log::{Log, LogError};
use crate::model::{Model, ModelRequest};
use crate::outbox::{self, Outbound};
use crate::payload::{Boundary, ModelPart, Parsed, ToolArgs, ToolOutput, measure, proposal};
use crate::projection::Projector;
use crate::rules_file;
use crate::seam::{Clock, Entropy, Tasks};
use crate::shim::{Barrier, barrier};
use crate::tool;

#[cfg(feature = "sim")]
mod planted;

/// Everything nondeterministic a session uses.
#[derive(Debug, Clone)]
pub struct Seams {
    pub model: Arc<dyn Model>,
    pub env: Arc<dyn Env>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    pub tasks: Tasks,
}

/// Configuration every session shares: data from main.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub limits: ServerLimits,
    /// The model's name, as the stream's `data-turn-start` reports it.
    pub model: String,
    /// The system prompt.
    pub system: String,
    /// A deliberate defect a simulation plants to show its checks catch it.
    /// Always [`Planted::None`] without the `sim` feature.
    pub planted: Planted,
}

/// A deliberate defect, for the simulation's mutation tests. Without the
/// `sim` feature there is none: the shipped daemon cannot plant one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planted {
    None,
    /// Start tool runs and summarize calls before their decision is
    /// durable (breaks T1: write before you act). The model's replay must
    /// catch the run, or the summarize call, it has not started.
    #[cfg(feature = "sim")]
    RunBeforePersist,
    /// Run each tool twice, reporting once (breaks at-most-once inside one
    /// dispatch). The replay must catch the second act.
    #[cfg(feature = "sim")]
    RunTwice,
    /// Persist an `Unknown` result as `NotStarted` (breaks T3 in the shell,
    /// below the core). The replay must catch the durable records differing
    /// from the decided ones.
    #[cfg(feature = "sim")]
    UnknownAsNotStarted,
    /// Report a command stopped mid-run as `NotStarted`, as `Interrupted`
    /// did before T3: a run that may have acted claims it never began. The
    /// replay must catch the report of a run that acted.
    #[cfg(feature = "sim")]
    StoppedAsNotStarted,
    /// Start a decision's effects although the log refused its records
    /// (breaks T1's fail-stop: nothing acts without a durable record).
    #[cfg(feature = "sim")]
    ActAfterRefusal,
    /// Arm no deadline (breaks P1): a run that never reports is waited on
    /// for good.
    #[cfg(feature = "sim")]
    NoDeadline,
    /// Acknowledge a delivery before its `Delivered` is durable (breaks T1
    /// and C2): a sender may record `Ok` for an envelope its recipient
    /// never holds.
    #[cfg(feature = "sim")]
    AckBeforeAppend,
    /// Mint a fresh envelope id when a `Keyed` call is run again after a
    /// crash (breaks C2): the recipient takes it twice.
    #[cfg(feature = "sim")]
    FreshKeyOnResume,
    /// Deliver a `send` or `spawn` before its `Dispatched` is durable
    /// (breaks C1 between logs).
    #[cfg(feature = "sim")]
    DeliverBeforePersist,
    /// Count a message consumed when it is injected into a running turn,
    /// not when a model call carrying it starts (breaks T4): when that turn
    /// ends before carrying it (Esc), the turn it would start is dropped,
    /// and the message is never read.
    #[cfg(feature = "sim")]
    ConsumeAtInjection,
    /// Drop a stop's delivery to a child and report it done (breaks C2 for
    /// a stop): the child runs on though its parent stopped.
    #[cfg(feature = "sim")]
    StopSkipsChildren,
    /// Render a message where it was delivered, between a running step's
    /// `tool_use` and its results (breaks T4's well-formed history).
    #[cfg(feature = "sim")]
    SpliceSteering,
    /// Never tell the model a task's end, though the step that carries it
    /// records it consumed (breaks T4: the model never sees it).
    #[cfg(feature = "sim")]
    UntoldTaskEnds,
    /// Take the mail no step carried yet into a compaction's summary
    /// (breaks T4: unread mail inside a cut). The step that carries it
    /// never tells it.
    #[cfg(feature = "sim")]
    CutSwallowsMail,
    /// Cut the history between the last step's `tool_use` and its results
    /// (breaks T4's well-formed history). The core only ever cuts at a
    /// beat; this guards a history that keeps recent turns verbatim.
    #[cfg(feature = "sim")]
    CutMidStep,
    /// Acknowledge a group commit without its fsync (breaks T1): a crash
    /// loses records the shell acted on. The simulation's log store
    /// plants it ([`crate::log::FileLogStore::skipping_sync`]).
    #[cfg(feature = "sim")]
    SkipFsync,
}

/// Why a session stopped. Every variant is fatal: the daemon exits and
/// recovery is the way back.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("the log does not replay: record {} breaks {:?}", .0.seq, .0.violation)]
    Corrupt(Corrupt),
}

/// The session stopped: its task is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the session is not running")]
pub struct Unavailable;

/// Why a command did not take effect.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error(transparent)]
    Rejected(#[from] Rejection),
    #[error(transparent)]
    Unavailable(#[from] Unavailable),
}

/// A read of the session stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRead {
    pub chunks: Vec<UiChunk>,
    /// Where the next read starts.
    pub next: Offset,
    pub freshness: Freshness,
}

type Reply<T> = oneshot::Sender<Result<T, Rejection>>;

enum Input {
    Deliver {
        envelope: Envelope<Parsed>,
        reply: oneshot::Sender<Outcome>,
    },
    Born {
        born: Born,
        reply: oneshot::Sender<Outcome>,
    },
    Message {
        command: SendMessage,
        reply: Reply<SendMessageReply>,
    },
    Approve {
        command: Approve,
        reply: Reply<ApproveReply>,
    },
    Cancel {
        command: Cancel,
        reply: Reply<CancelReply>,
    },
    Compact {
        command: Compact,
        reply: Reply<CompactReply>,
    },
    Read {
        from: Offset,
        reply: oneshot::Sender<StreamRead>,
    },
    Records {
        reply: oneshot::Sender<Vec<Record<Parsed>>>,
    },
    Summary {
        reply: oneshot::Sender<Option<ActorSummary>>,
    },
    Done(Event<Parsed>),
}

/// The seq the next record of a session will get, for stream followers to
/// wait on.
///
/// Like a `tokio::sync::watch`, except for one thing a simulation needs:
/// `watch` files each waiter under a notifier picked by tokio's per-process
/// random number, so followers woken by one commit run in an order that
/// changes from process to process. A [`Notify`] wakes them in the order
/// they started waiting, so a simulated run replays exactly.
#[derive(Clone)]
pub struct Committed {
    shared: Arc<CommittedState>,
}

struct CommittedState {
    #[expect(
        clippy::disallowed_types,
        reason = "written only by the session task after each commit (it only grows); \
                  read by stream followers; dropped with the last follower"
    )]
    next: core::sync::atomic::AtomicU64,
    #[expect(
        clippy::disallowed_types,
        reason = "cleared once, by the session task as it stops; read by stream followers"
    )]
    open: core::sync::atomic::AtomicBool,
    notify: Notify,
}

impl fmt::Debug for Committed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Committed")
            .field("next", &self.get())
            .finish_non_exhaustive()
    }
}

impl Committed {
    #[expect(
        clippy::disallowed_types,
        reason = "the atomics described on CommittedState"
    )]
    fn new(next: u64) -> (Self, CommittedWriter) {
        let committed = Self {
            shared: Arc::new(CommittedState {
                next: core::sync::atomic::AtomicU64::new(next),
                open: core::sync::atomic::AtomicBool::new(true),
                notify: Notify::new(),
            }),
        };
        (committed.clone(), CommittedWriter { committed })
    }

    /// The seq the next record will get.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.shared.next.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Waits until a record at `seq` is committed, and returns the seq the
    /// next record will get.
    ///
    /// # Errors
    /// The session stopped first.
    pub async fn past(&self, seq: u64) -> Result<u64, Unavailable> {
        loop {
            let notified = self.shared.notify.notified();
            let mut notified = core::pin::pin!(notified);
            notified.as_mut().enable();
            let next = self.get();
            if next > seq {
                return Ok(next);
            }
            if !self.shared.open.load(core::sync::atomic::Ordering::Acquire) {
                return Err(Unavailable);
            }
            notified.await;
        }
    }
}

/// The session's side of its [`Committed`]; dropping it tells followers the
/// session stopped.
struct CommittedWriter {
    committed: Committed,
}

impl CommittedWriter {
    fn set(&self, next: u64) {
        self.committed
            .shared
            .next
            .store(next, core::sync::atomic::Ordering::Release);
        self.committed.shared.notify.notify_waiters();
    }
}

impl Drop for CommittedWriter {
    fn drop(&mut self) {
        self.committed
            .shared
            .open
            .store(false, core::sync::atomic::Ordering::Release);
        self.committed.shared.notify.notify_waiters();
    }
}

/// How the rest of the daemon talks to one session.
#[derive(Clone)]
pub struct ActorHandle {
    session: ActorId,
    inbox: mpsc::Sender<Input>,
    committed: Committed,
    /// One permit per live stream follower (`sse_followers_per_actor`).
    followers: Arc<Semaphore>,
}

impl fmt::Debug for ActorHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActorHandle")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl ActorHandle {
    #[must_use]
    pub const fn actor(&self) -> ActorId {
        self.session
    }

    /// Delivers `envelope` to this actor's mailbox, answered once its
    /// `Delivered` is durable (T1): `Accepted`, `Exists` for an id it
    /// already holds (C2), or why it was refused. A full log refuses it as
    /// `Rejected(Limit)`.
    ///
    /// # Errors
    /// The actor is gone (the daemon is stopping).
    pub async fn deliver(&self, envelope: Envelope<Parsed>) -> Result<Outcome, Unavailable> {
        self.ask(|reply| Input::Deliver { envelope, reply }).await
    }

    /// Records who this actor is, answered once it is durable: `Accepted`,
    /// `Exists` once it is born, or why not.
    ///
    /// # Errors
    /// The actor is gone (the daemon is stopping).
    pub async fn born(&self, born: Born) -> Result<Outcome, Unavailable> {
        self.ask(|reply| Input::Born { born, reply }).await
    }

    /// Your stop: revokes everything this actor holds, and stops every
    /// actor it spawned (C3). `Exists` on a repeat of `stop`.
    ///
    /// # Errors
    /// The actor is gone (the daemon is stopping).
    pub async fn stop(&self, stop: MessageId) -> Result<Outcome, Unavailable> {
        self.deliver(Envelope {
            id: EnvelopeId::User(stop),
            from: Address::User,
            to: self.session,
            reply_to: None,
            body: Body::Stop,
        })
        .await
    }

    /// Your stop, as a command: answered once it is durably in the
    /// mailbox (`Created`, or `Exists` on a repeat of `command`).
    ///
    /// # Errors
    /// The actor is stopped already (by another stop), its log is full, or
    /// the actor is gone.
    pub async fn stop_actor(&self, command: StopActor) -> Result<StopReply, CommandError> {
        let outcome = match self.stop(command.stop_id).await? {
            Outcome::Accepted => WireOutcome::Created,
            Outcome::Exists => WireOutcome::Exists,
            Outcome::Rejected(CoreRejection::Stopped) => {
                return Err(Rejection::ActorStopped {
                    actor_id: self.session,
                }
                .into());
            }
            Outcome::Rejected(CoreRejection::Limit) => {
                return Err(Rejection::ActorFull {
                    actor_id: self.session,
                    reason: "its log takes no more records".to_owned(),
                }
                .into());
            }
            Outcome::Rejected(CoreRejection::Unborn) => {
                return Err(Rejection::UnknownActor {
                    actor_id: self.session,
                }
                .into());
            }
            Outcome::Rejected(
                CoreRejection::Mismatch
                | CoreRejection::UnknownCall
                | CoreRejection::NotRunning
                | CoreRejection::Running,
            )
            | Outcome::Stale => defect("a stop is accepted, a repeat, or refused"),
        };
        Ok(StopReply { outcome })
    }

    async fn ask<T, F>(&self, make: F) -> Result<T, Unavailable>
    where
        F: FnOnce(oneshot::Sender<T>) -> Input,
    {
        let (reply, answer) = oneshot::channel();
        self.inbox
            .send(make(reply))
            .await
            .map_err(|_closed| Unavailable)?;
        answer.await.map_err(|_dropped| Unavailable)
    }

    /// # Errors
    /// The daemon refused the message, or the session is gone.
    pub async fn send_message(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageReply, CommandError> {
        Ok(self
            .ask(|reply| Input::Message { command, reply })
            .await??)
    }

    /// # Errors
    /// The daemon refused the verdict, or the session is gone.
    pub async fn approve(&self, command: Approve) -> Result<ApproveReply, CommandError> {
        Ok(self
            .ask(|reply| Input::Approve { command, reply })
            .await??)
    }

    /// # Errors
    /// Nothing to cancel, or the session is gone.
    pub async fn cancel(&self, command: Cancel) -> Result<CancelReply, CommandError> {
        Ok(self.ask(|reply| Input::Cancel { command, reply }).await??)
    }

    /// Your `/compact`: summarizes the actor's history between turns.
    ///
    /// # Errors
    /// A turn is running, the actor is stopped or full, or the session is
    /// gone.
    pub async fn compact(&self, command: Compact) -> Result<CompactReply, CommandError> {
        Ok(self
            .ask(|reply| Input::Compact { command, reply })
            .await??)
    }

    /// Chunks from `from` to the end of the stream (bounded per read).
    ///
    /// # Errors
    /// The session is gone.
    pub async fn read(&self, from: Offset) -> Result<StreamRead, Unavailable> {
        self.ask(|reply| Input::Read { from, reply }).await
    }

    /// Every committed record, for world-level checkers.
    ///
    /// # Errors
    /// The session is gone.
    pub async fn records(&self) -> Result<Vec<Record<Parsed>>, Unavailable> {
        self.ask(|reply| Input::Records { reply }).await
    }

    /// Where this actor is, from its live core and projection: `None`
    /// while it is unborn.
    ///
    /// # Errors
    /// The session is gone.
    pub async fn summary(&self) -> Result<Option<ActorSummary>, Unavailable> {
        self.ask(|reply| Input::Summary { reply }).await
    }

    /// The seq the next record will get; changes after every commit.
    #[must_use]
    pub fn committed(&self) -> Committed {
        self.committed.clone()
    }

    /// A slot for one more live follower of this session, held until the
    /// follower ends; `None` when the session serves its limit.
    #[must_use]
    pub fn follower(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.followers).try_acquire_owned().ok()
    }
}

/// One session's shell: owns the core, the log and the projected stream.
pub struct Session {
    id: ActorId,
    core: Core<Parsed>,
    log: Box<dyn Log>,
    records: Vec<Record<Parsed>>,
    chunks: Vec<Projected>,
    projector: Projector,
    seams: Seams,
    config: Arc<ShellConfig>,
    inbox: mpsc::Sender<Input>,
    committed: CommittedWriter,
    /// Who the actor is, for its first resume to record (a new log); `None`
    /// for one recovered at boot.
    born: Option<Born>,
    /// Where deliveries to other actors go.
    registry: Registry,
    cancels: BTreeMap<EffectId, CancellationToken>,
    /// The deadline of each run in flight (P1): cancelled when it reports.
    deadlines: BTreeMap<EffectId, CancellationToken>,
    /// Each child this session's spawn delivery last ran for, and a token
    /// cancelled once that delivery is done: a stop to the child waits for
    /// it, so it never finds the child unborn while the spawn may still
    /// birth it.
    spawning: BTreeMap<ActorId, CancellationToken>,
    /// JSON bytes of `records` and `chunks`, against `actor_bytes`.
    held_bytes: usize,
    /// Why the log stopped taking records, once it has: from then on the
    /// session fails stop (T1).
    full: Option<String>,
}

/// What persisting a decision came to.
enum Persisted {
    /// Durable and applied: the outcome, and the effects to start.
    Done(Outcome, Vec<Effect<Parsed>>),
    /// The log refused it (nothing written, nothing applied): the session
    /// is full.
    Refused,
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("id", &self.id)
            .field("seq", &self.core.seq())
            .finish_non_exhaustive()
    }
}

/// The task that runs a session; spawn it through [`Tasks`].
pub type Running = Pin<Box<dyn Future<Output = Result<(), ShellError>> + Send>>;

impl Session {
    /// Recovers a session from its log's records and returns its handle and
    /// the task to run. The task first resumes (bumping the epoch and
    /// restarting or resolving whatever was in flight).
    ///
    /// # Errors
    /// The records do not replay.
    pub fn start(
        session: ActorId,
        log: Box<dyn Log>,
        records: Vec<Record<Parsed>>,
        born: Option<Born>,
        registry: Registry,
        seams: Seams,
        config: Arc<ShellConfig>,
    ) -> Result<(ActorHandle, Running), ShellError> {
        let core =
            Core::recover(config.limits.core.clone(), &records).map_err(ShellError::Corrupt)?;
        let mut projector = Projector::new(config.model.clone());
        let mut held_bytes: usize = 0;
        let mut chunks = Vec::with_capacity(records.len());
        for record in &records {
            let projected = Projected::new(projector.project(record));
            held_bytes = held_bytes
                .checked_add(held(record, &projected))
                .unwrap_or_else(|| defect("a session's bytes fit in usize"));
            chunks.push(projected);
        }
        let (inbox, receiver) = mpsc::channel(config.limits.actor_inbox);
        let (committed, writer) = Committed::new(core.seq());
        let handle = ActorHandle {
            session,
            inbox: inbox.clone(),
            committed,
            followers: Arc::new(Semaphore::new(config.limits.sse_followers_per_actor)),
        };
        let shell = Self {
            id: session,
            core,
            log,
            records,
            chunks,
            projector,
            seams,
            config,
            inbox,
            committed: writer,
            born,
            registry,
            cancels: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            spawning: BTreeMap::new(),
            held_bytes,
            full: None,
        };
        Ok((handle, Box::pin(shell.run(receiver))))
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<Input>) -> Result<(), ShellError> {
        if let Persisted::Done(_, effects) = self.resume().await? {
            self.start_effects(effects);
        }
        let shutdown = self.seams.tasks.shutdown().clone();
        loop {
            let input = tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(()),
                input = receiver.recv() => input,
            };
            let Some(input) = input else {
                return Ok(());
            };
            self.handle(input).await?;
        }
    }

    async fn resume(&mut self) -> Result<Persisted, ShellError> {
        // Who the actor is is recorded at its first resume; the rules file
        // is read at every boot.
        let load = self.rules_changed().await;
        let resumed = self.core.resume(self.born.take(), load);
        #[cfg(feature = "sim")]
        crate::shim::observe(crate::shim::Observation::Resumed {
            actor: self.id,
            records: resumed.records.clone(),
        });
        self.persist(resumed).await
    }

    /// Steps 2 and 3: durable, then applied and projected. Once the log has
    /// refused an append, nothing more is (fail-stop).
    async fn persist(&mut self, decision: Decision<Parsed>) -> Result<Persisted, ShellError> {
        let Decision {
            records,
            effects,
            outcome,
        } = decision;
        // The planted defects that tamper with a decision.
        #[cfg(feature = "sim")]
        let (records, effects) = self.planted_decision(records, effects);
        if !records.is_empty() {
            assert!(
                self.full.is_none() && self.log.next_seq() == self.core.seq(),
                "a log that refused nothing, agreeing with the core on the next seq"
            );
            match self.log.append(&records).await {
                Ok(()) => {}
                Err(error) if error.refused() => {
                    self.halt(error.to_string());
                    // The planted defect: act on what is not durable.
                    #[cfg(feature = "sim")]
                    if self.config.planted == Planted::ActAfterRefusal {
                        self.start_effects(effects);
                    }
                    return Ok(refused(self.id));
                }
                Err(error) => return Err(error.into()),
            }
            for record in records {
                if let Err(corrupt) = self.core.apply(&record) {
                    defect(&format!("apply refused a decided record: {corrupt:?}"));
                }
                let projected = Projected::new(self.projector.project(&record));
                self.held_bytes = self
                    .held_bytes
                    .checked_add(held(&record, &projected))
                    .unwrap_or_else(|| defect("a session's bytes fit in usize"));
                self.chunks.push(projected);
                self.records.push(record);
            }
            self.committed.set(self.core.seq());
        }
        Ok(Persisted::Done(outcome, effects))
    }

    /// Decides `event` and persists the decision; once the log is full,
    /// decides nothing (fail-stop).
    async fn decide(&mut self, event: Event<Parsed>) -> Result<Persisted, ShellError> {
        if self.full.is_some() {
            return Ok(Persisted::Refused);
        }
        #[cfg(feature = "sim")]
        let observed = event.clone();
        let decision = self.core.decide(event);
        #[cfg(feature = "sim")]
        crate::shim::observe(crate::shim::Observation::Decided {
            actor: self.id,
            event: observed,
            records: decision.records.clone(),
        });
        self.persist(decision).await
    }

    /// The log refused an append: the session fails stop (T1). It records
    /// why, and asks every model call and run it holds to stop, since
    /// nothing they say could be recorded. Each was dispatched durably;
    /// what it reports is dropped, and a restart resolves it from the log.
    fn halt(&mut self, why: String) {
        self.full.get_or_insert(why);
        for cancel in self.cancels.values() {
            cancel.cancel();
        }
    }

    /// The rules file as it is now, if its text differs from the text in
    /// force (or no rules were ever loaded): what to record (C1).
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "`&mut` keeps the future `Send`: a session is not `Sync` (its log is not)"
    )]
    async fn rules_changed(&mut self) -> Option<Loaded<Parsed>> {
        let env = Arc::clone(&self.seams.env);
        let config = Arc::clone(&self.config);
        let loaded = rules_file::load(env.as_ref(), &config.limits).await;
        self.differs(loaded)
    }

    /// `loaded`, if its text differs from the rules file's text in force.
    fn differs(&self, loaded: Loaded<Parsed>) -> Option<Loaded<Parsed>> {
        let Some((seq, _)) = self.core.rules() else {
            return Some(loaded);
        };
        let in_force = usize::try_from(*seq)
            .ok()
            .and_then(|at| self.records.get(at))
            .and_then(|record| match record {
                Record::RulesLoaded { file, .. } => Some(file.as_ref().map(|file| &file.body)),
                Record::Born { .. }
                | Record::Delivered { .. }
                | Record::TurnStarted { .. }
                | Record::StepStarted { .. }
                | Record::ModelOutput { .. }
                | Record::StepReset { .. }
                | Record::StepFinished { .. }
                | Record::Approved { .. }
                | Record::Dispatched { .. }
                | Record::ToolResult { .. }
                | Record::TaskEnded { .. }
                | Record::StopSent { .. }
                | Record::CancelRequested { .. }
                | Record::TurnEnded { .. }
                | Record::EpochBumped { .. }
                | Record::CompactionStarted { .. }
                | Record::Compacted { .. } => None,
            })
            .unwrap_or_else(|| defect("the rules in force are a RulesLoaded in the log"));
        (in_force != loaded.file.as_ref().map(|file| &file.body)).then_some(loaded)
    }

    /// The answer to every command once the log is full.
    fn full(&self) -> Rejection {
        Rejection::ActorFull {
            actor_id: self.id,
            reason: format!(
                "its log takes no more records ({})",
                self.full.as_deref().unwrap_or("full")
            ),
        }
    }

    async fn handle(&mut self, input: Input) -> Result<(), ShellError> {
        match input {
            Input::Done(event) => {
                match &event {
                    Event::ModelDone { id, .. } | Event::Summarized { id, .. } => {
                        self.cancels.remove(id);
                    }
                    Event::ToolDone { id, .. } => {
                        self.cancels.remove(id);
                        if let Some(deadline) = self.deadlines.remove(id) {
                            deadline.cancel();
                        }
                    }
                    // The run is still asked to stop (`CancelTool`), so
                    // its token stays until it reports.
                    Event::Expired { id } => {
                        self.deadlines.remove(id);
                    }
                    Event::Born { .. }
                    | Event::Rules { .. }
                    | Event::Deliver { .. }
                    | Event::Approve { .. }
                    | Event::Interrupt { .. }
                    | Event::ModelParts { .. }
                    | Event::Woke { .. }
                    | Event::Compact => {}
                }
                // A model's response is settled under the rules file as it
                // is now: its calls are granted then (C1).
                if matches!(event, Event::ModelDone { .. })
                    && let Persisted::Refused = self.reload_rules().await?
                {
                    return Ok(());
                }
                if let Persisted::Done(_, effects) = self.decide(event).await? {
                    self.start_effects(effects);
                }
            }
            Input::Deliver { envelope, reply } => {
                // The planted defect: acknowledge before the append.
                #[cfg(feature = "sim")]
                let reply = if self.config.planted == Planted::AckBeforeAppend {
                    answer(reply, Outcome::Accepted);
                    None
                } else {
                    Some(reply)
                };
                #[cfg(not(feature = "sim"))]
                let reply = Some(reply);
                let (outcome, effects) = match self.deliver(envelope).await? {
                    Persisted::Done(outcome, effects) => (outcome, effects),
                    // A full log takes no mail: the sender hears a limit.
                    Persisted::Refused => (Outcome::Rejected(CoreRejection::Limit), Vec::new()),
                };
                if let Some(reply) = reply {
                    answer(reply, outcome);
                }
                self.start_effects(effects);
            }
            Input::Born { born, reply } => {
                let (outcome, effects) = match self.decide(Event::Born { born }).await? {
                    Persisted::Done(outcome, effects) => (outcome, effects),
                    Persisted::Refused => (Outcome::Rejected(CoreRejection::Limit), Vec::new()),
                };
                answer(reply, outcome);
                self.start_effects(effects);
            }
            Input::Message { command, reply } => {
                let (answer, effects) = self.message(command).await?;
                drop(reply.send(answer));
                self.start_effects(effects);
            }
            Input::Approve { command, reply } => {
                let verdict = match command.verdict {
                    WireVerdict::Grant => Verdict::Approve,
                    WireVerdict::Deny => Verdict::Deny,
                };
                let event = Event::Approve {
                    call: command.call_id.clone(),
                    verdict,
                };
                match self.decide(event).await? {
                    Persisted::Done(outcome, effects) => {
                        let answer = self.approve_answer(outcome, &command);
                        drop(reply.send(answer));
                        self.start_effects(effects);
                    }
                    Persisted::Refused => drop(reply.send(Err(self.full()))),
                }
            }
            Input::Cancel { command, reply } => {
                let turn = command.turn_id;
                match self.decide(Event::Interrupt { turn }).await? {
                    Persisted::Done(outcome, effects) => {
                        let answer = self.cancel_answer(outcome, turn);
                        drop(reply.send(answer));
                        self.start_effects(effects);
                    }
                    Persisted::Refused => drop(reply.send(Err(self.full()))),
                }
            }
            Input::Compact {
                command: Compact {},
                reply,
            } => match self.decide(Event::Compact).await? {
                Persisted::Done(outcome, effects) => {
                    drop(reply.send(self.compact_answer(outcome)));
                    self.start_effects(effects);
                }
                Persisted::Refused => drop(reply.send(Err(self.full()))),
            },
            Input::Read { from, reply } => drop(reply.send(self.read(from))),
            Input::Records { reply } => drop(reply.send(self.records.clone())),
            Input::Summary { reply } => drop(reply.send(crate::projection::summary(
                self.id,
                &self.core,
                &self.projector,
            ))),
        }
        Ok(())
    }

    /// Records the rules file if its text differs from the text in force
    /// (C1): a change takes effect from the next decision on. Refused once
    /// the log is full.
    async fn reload_rules(&mut self) -> Result<Persisted, ShellError> {
        let Some(Loaded { rules, file }) = self.rules_changed().await else {
            return Ok(Persisted::Done(Outcome::Accepted, Vec::new()));
        };
        let persisted = self.decide(Event::Rules { rules, file }).await?;
        if let Persisted::Done(_, effects) = persisted {
            self.start_effects(effects);
            return Ok(Persisted::Done(Outcome::Accepted, Vec::new()));
        }
        Ok(persisted)
    }

    /// Delivers `envelope`, from anyone (T1: durable, then answered). The
    /// rules file is read first: whatever it starts runs under the rules as
    /// they are now (C1).
    async fn deliver(&mut self, envelope: Envelope<Parsed>) -> Result<Persisted, ShellError> {
        if let Persisted::Refused = self.reload_rules().await? {
            return Ok(Persisted::Refused);
        }
        self.decide(Event::Deliver { envelope }).await
    }

    /// Your message, delivered as any envelope is, and the answer to it.
    /// It is refused before anything is recorded when the actor holds its
    /// limit of bytes, or is idle at its limit of turns, so an error never
    /// answers a message the log holds.
    async fn message(
        &mut self,
        command: SendMessage,
    ) -> Result<(Result<SendMessageReply, Rejection>, Vec<Effect<Parsed>>), ShellError> {
        if self.full.is_some() {
            return Ok((Err(self.full()), Vec::new()));
        }
        if self.delivered(command.message_id).is_none() {
            if self.held_bytes >= self.config.limits.actor_bytes {
                let reason = format!(
                    "it holds {} bytes, its limit is {}",
                    self.held_bytes, self.config.limits.actor_bytes
                );
                return Ok((Err(self.actor_full(reason)), Vec::new()));
            }
            let turns =
                u64::try_from(self.config.limits.core.turns_per_actor()).unwrap_or(u64::MAX);
            if self.core.running().is_none()
                && self.core.turns() >= turns
                && self.core.born().is_some()
                && !self.core.stopped()
            {
                let reason = format!("it started its limit of {turns} turns");
                return Ok((Err(self.actor_full(reason)), Vec::new()));
            }
        }
        let text = command.text.as_str().to_owned();
        let envelope = Envelope {
            id: EnvelopeId::User(command.message_id),
            from: Address::User,
            to: self.id,
            reply_to: None,
            body: Body::Text(measure(text.clone())),
        };
        Ok(match self.deliver(envelope).await? {
            Persisted::Done(outcome, effects) => {
                (self.message_answer(outcome, &command, &text), effects)
            }
            Persisted::Refused => (Err(self.full()), Vec::new()),
        })
    }

    const fn actor_full(&self, reason: String) -> Rejection {
        Rejection::ActorFull {
            actor_id: self.id,
            reason,
        }
    }

    fn message_answer(
        &self,
        outcome: Outcome,
        command: &SendMessage,
        text: &str,
    ) -> Result<SendMessageReply, Rejection> {
        let created = match outcome {
            Outcome::Accepted => WireOutcome::Created,
            Outcome::Exists => WireOutcome::Exists,
            Outcome::Rejected(CoreRejection::Limit) => {
                return Err(Rejection::ActorFull {
                    actor_id: self.id,
                    reason: format!(
                        "it holds its limit of {} messages",
                        self.config.limits.core.messages_per_actor()
                    ),
                });
            }
            Outcome::Rejected(CoreRejection::Stopped) => {
                return Err(Rejection::ActorStopped { actor_id: self.id });
            }
            // A log a spawn made whose birth a crash tore, and that no
            // spawn will birth: no actor lives there.
            Outcome::Rejected(CoreRejection::Unborn) => {
                return Err(Rejection::UnknownActor { actor_id: self.id });
            }
            Outcome::Rejected(
                CoreRejection::Mismatch
                | CoreRejection::UnknownCall
                | CoreRejection::NotRunning
                | CoreRejection::Running,
            )
            | Outcome::Stale => {
                defect("a message is accepted, a repeat, full, stopped or to no actor")
            }
        };
        let Some((at, original)) = self.delivered(command.message_id) else {
            defect("the core took a message the log does not hold");
        };
        if original.body != text {
            return Err(Rejection::MessageExistsWithDifferentText {
                message_id: command.message_id,
            });
        }
        // The turn the message landed in: the one running at its delivery,
        // which it steers, or else the first after it, which it started.
        let running = self.records.get(..at).and_then(|before| {
            before.iter().rev().find_map(|record| match record {
                Record::TurnStarted { turn } => Some(Some(*turn)),
                Record::TurnEnded { .. } => Some(None),
                Record::Born { .. }
                | Record::RulesLoaded { .. }
                | Record::Delivered { .. }
                | Record::StepStarted { .. }
                | Record::ModelOutput { .. }
                | Record::StepReset { .. }
                | Record::StepFinished { .. }
                | Record::Approved { .. }
                | Record::Dispatched { .. }
                | Record::ToolResult { .. }
                | Record::TaskEnded { .. }
                | Record::StopSent { .. }
                | Record::CancelRequested { .. }
                | Record::EpochBumped { .. }
                | Record::CompactionStarted { .. }
                | Record::Compacted { .. } => None,
            })
        });
        let turn = running.flatten().or_else(|| {
            self.records.get(at..).and_then(|after| {
                after.iter().find_map(|record| match record {
                    Record::TurnStarted { turn } => Some(*turn),
                    Record::Born { .. }
                    | Record::RulesLoaded { .. }
                    | Record::Delivered { .. }
                    | Record::StepStarted { .. }
                    | Record::ModelOutput { .. }
                    | Record::StepReset { .. }
                    | Record::StepFinished { .. }
                    | Record::Approved { .. }
                    | Record::Dispatched { .. }
                    | Record::ToolResult { .. }
                    | Record::TaskEnded { .. }
                    | Record::StopSent { .. }
                    | Record::CancelRequested { .. }
                    | Record::TurnEnded { .. }
                    | Record::EpochBumped { .. }
                    | Record::CompactionStarted { .. }
                    | Record::Compacted { .. } => None,
                })
            })
        });
        match turn {
            Some(turn) => Ok(SendMessageReply {
                turn_id: turn,
                outcome: created,
            }),
            // A repeat of a message a daemon from before the turn check
            // took at the limit: it is in the mailbox, and no turn carries
            // it.
            None => Err(self.actor_full(format!(
                "it started its limit of {} turns",
                self.config.limits.core.turns_per_actor()
            ))),
        }
    }

    /// Where the log holds `message`'s delivery, and its text.
    fn delivered(&self, message: MessageId) -> Option<(usize, &Measured<String>)> {
        self.records
            .iter()
            .enumerate()
            .find_map(|(at, record)| match record {
                Record::Delivered {
                    envelope:
                        Envelope {
                            id: EnvelopeId::User(seen),
                            body: Body::Text(text),
                            ..
                        },
                } if *seen == message => Some((at, text)),
                Record::Born { .. }
                | Record::RulesLoaded { .. }
                | Record::Delivered { .. }
                | Record::TurnStarted { .. }
                | Record::StepStarted { .. }
                | Record::ModelOutput { .. }
                | Record::StepReset { .. }
                | Record::StepFinished { .. }
                | Record::Approved { .. }
                | Record::Dispatched { .. }
                | Record::ToolResult { .. }
                | Record::TaskEnded { .. }
                | Record::StopSent { .. }
                | Record::CancelRequested { .. }
                | Record::TurnEnded { .. }
                | Record::EpochBumped { .. }
                | Record::CompactionStarted { .. }
                | Record::Compacted { .. } => None,
            })
    }

    /// The answer to `command`. A verdict for no call awaiting one is a
    /// repeat only if the log holds the same verdict for that call.
    fn approve_answer(
        &self,
        outcome: Outcome,
        command: &Approve,
    ) -> Result<ApproveReply, Rejection> {
        match outcome {
            Outcome::Accepted => Ok(ApproveReply {
                outcome: WireOutcome::Created,
            }),
            Outcome::Rejected(CoreRejection::UnknownCall) => {
                match verdict_of(&self.records, &command.call_id) {
                    Some(verdict) if verdict == command.verdict => Ok(ApproveReply {
                        outcome: WireOutcome::Exists,
                    }),
                    Some(verdict) => Err(Rejection::ApprovalExistsWithDifferentVerdict {
                        call_id: command.call_id.clone(),
                        verdict,
                    }),
                    None => Err(Rejection::NoPendingApproval {
                        call_id: command.call_id.clone(),
                    }),
                }
            }
            Outcome::Rejected(CoreRejection::Stopped) => {
                Err(Rejection::ActorStopped { actor_id: self.id })
            }
            Outcome::Exists
            | Outcome::Rejected(
                CoreRejection::Limit
                | CoreRejection::Unborn
                | CoreRejection::Mismatch
                | CoreRejection::NotRunning
                | CoreRejection::Running,
            )
            | Outcome::Stale => {
                defect("a verdict is accepted, for no call awaiting one, or stopped")
            }
        }
    }

    fn cancel_answer(&self, outcome: Outcome, turn_id: TurnId) -> Result<CancelReply, Rejection> {
        match outcome {
            Outcome::Accepted => Ok(CancelReply {
                turn_id,
                outcome: WireOutcome::Created,
            }),
            Outcome::Exists => Ok(CancelReply {
                turn_id,
                outcome: WireOutcome::Exists,
            }),
            Outcome::Rejected(CoreRejection::NotRunning) => {
                if was_cancelled(&self.records, turn_id) {
                    Ok(CancelReply {
                        turn_id,
                        outcome: WireOutcome::Exists,
                    })
                } else {
                    Err(Rejection::TurnNotRunning { turn_id })
                }
            }
            Outcome::Rejected(CoreRejection::Stopped) => {
                Err(Rejection::ActorStopped { actor_id: self.id })
            }
            Outcome::Rejected(
                CoreRejection::Limit
                | CoreRejection::Unborn
                | CoreRejection::Mismatch
                | CoreRejection::UnknownCall
                | CoreRejection::Running,
            )
            | Outcome::Stale => {
                defect("an interrupt is accepted, a repeat, not running or stopped")
            }
        }
    }

    fn compact_answer(&self, outcome: Outcome) -> Result<CompactReply, Rejection> {
        match outcome {
            Outcome::Accepted => Ok(CompactReply {
                outcome: WireOutcome::Created,
            }),
            Outcome::Rejected(CoreRejection::Running) => {
                Err(Rejection::TurnRunning { actor_id: self.id })
            }
            Outcome::Rejected(CoreRejection::Stopped) => {
                Err(Rejection::ActorStopped { actor_id: self.id })
            }
            Outcome::Rejected(CoreRejection::Unborn) => {
                Err(Rejection::UnknownActor { actor_id: self.id })
            }
            Outcome::Exists
            | Outcome::Rejected(
                CoreRejection::Limit
                | CoreRejection::Mismatch
                | CoreRejection::UnknownCall
                | CoreRejection::NotRunning,
            )
            | Outcome::Stale => {
                defect("a compact is accepted, or refused while running, stopped or unborn")
            }
        }
    }

    fn read(&self, from: Offset) -> StreamRead {
        let tail = self.core.seq();
        let start = from.seq().min(tail);
        // Records' arrays merged into one cost at most their lengths
        // together. `ServerLimits::check` makes sure one record's fits.
        let budget = self.config.limits.wire.max_data_bytes();
        let mut chunks = Vec::new();
        let mut bytes: usize = 0;
        let mut next = start;
        while next < tail {
            let index = usize::try_from(next).expect("records in memory fit in usize");
            let Some(projected) = self.chunks.get(index) else {
                break;
            };
            let grown = bytes.saturating_add(projected.bytes);
            if next > start && grown > budget {
                break;
            }
            chunks.extend(projected.chunks.iter().cloned());
            bytes = grown;
            next = next.saturating_add(1);
        }
        StreamRead {
            chunks,
            next: Offset::at(next).expect("a session holds fewer than 10^16 records"),
            freshness: if next == tail {
                Freshness::UpToDate
            } else {
                Freshness::Behind
            },
        }
    }

    /// Step 5: every effect of a durable, applied decision.
    fn start_effects(&mut self, effects: Vec<Effect<Parsed>>) {
        for effect in effects {
            self.start_effect(effect);
        }
    }

    fn start_effect(&mut self, effect: Effect<Parsed>) {
        match effect {
            Effect::CallModel { id, .. } => {
                #[cfg(not(feature = "sim"))]
                let request = history::request(
                    &self.records,
                    &self.config.system,
                    self.config.limits.model_max_tokens,
                );
                // The history's planted defects, if one is planted.
                #[cfg(feature = "sim")]
                let request = history::planted(
                    &self.records,
                    &self.config.system,
                    self.config.limits.model_max_tokens,
                    self.config.planted,
                );
                #[cfg(feature = "sim")]
                crate::shim::observe(crate::shim::Observation::Requested {
                    actor: self.id,
                    id,
                    said: history::said(&request),
                });
                let cancel = CancellationToken::new();
                self.cancels.insert(id, cancel.clone());
                let task = ModelTask {
                    model: Arc::clone(&self.seams.model),
                    clock: Arc::clone(&self.seams.clock),
                    entropy: Arc::clone(&self.seams.entropy),
                    inbox: self.inbox.clone(),
                    config: Arc::clone(&self.config),
                    tools: request.tools.clone(),
                    id,
                    cancel,
                };
                self.seams.tasks.spawn(task.run(request));
            }
            Effect::CancelModel { id } | Effect::CancelTool { id } => {
                if let Some(cancel) = self.cancels.get(&id) {
                    cancel.cancel();
                }
            }
            Effect::AskApproval { request } => {
                // The stream already carries the approval request (projected
                // from `StepFinished`); the answer arrives as a command.
                drop::<ToolCall<_, Parsed>>(request);
            }
            Effect::RunTool { call } => self.run_call(call),
            Effect::Sleep { id, delay } => {
                let clock = Arc::clone(&self.seams.clock);
                let inbox = self.inbox.clone();
                self.seams.tasks.spawn(async move {
                    clock.sleep(delay).await;
                    report(&inbox, Event::Woke { id }).await;
                });
            }
            // v's housekeeping: the conversation so far, summarized under
            // the same bound as a step's call (one budget).
            Effect::Summarize { id } => {
                #[cfg(not(feature = "sim"))]
                let request = history::summarize(
                    &self.records,
                    &self.config.system,
                    self.config.limits.model_max_tokens,
                );
                #[cfg(feature = "sim")]
                let request = history::planted_summary(
                    &self.records,
                    &self.config.system,
                    self.config.limits.model_max_tokens,
                    self.config.planted,
                );
                #[cfg(feature = "sim")]
                crate::shim::observe(crate::shim::Observation::SummaryRequested {
                    actor: self.id,
                    id,
                    said: history::said_anywhere(&request),
                });
                let cancel = CancellationToken::new();
                self.cancels.insert(id, cancel.clone());
                let task = SummaryTask {
                    actor: self.id,
                    model: Arc::clone(&self.seams.model),
                    inbox: self.inbox.clone(),
                    config: Arc::clone(&self.config),
                    id,
                    cancel,
                };
                self.seams.tasks.spawn(task.run(request));
            }
            // A stop has no deadline: it is sent until it lands (C2). Its
            // only waits are its parent's spawn of the child, if one is in
            // flight, then the registry and the child's append.
            Effect::Stop { id, child } => {
                // The planted defect: the stop never reaches the child,
                // and says it did.
                #[cfg(feature = "sim")]
                if self.config.planted == Planted::StopSkipsChildren {
                    let inbox = self.inbox.clone();
                    self.seams.tasks.spawn(async move {
                        let output = measure(ToolOutput::complete(format!("stopped {child}")));
                        report(
                            &inbox,
                            Event::ToolDone {
                                id,
                                exit: Exit::Ok,
                                output,
                            },
                        )
                        .await;
                    });
                    return;
                }
                let envelope = Envelope {
                    id: EnvelopeId::Minted {
                        actor: self.id,
                        seq: id.seq,
                    },
                    from: Address::Runtime,
                    to: child,
                    reply_to: None,
                    body: Body::Stop,
                };
                let after = self.spawning.get(&child).cloned();
                self.send_out(
                    id,
                    Outbound::Send { envelope },
                    after,
                    CancellationToken::new(),
                );
            }
        }
    }

    /// Runs an approved call (a foreground run or a task's), and arms its
    /// deadline (P1).
    fn run_call(&mut self, call: ToolCall<Approved, Parsed>) {
        let id = call.effect();
        let deadline = self.config.limits.deadline(call.args());
        match call.args() {
            ToolArgs::Read { .. } | ToolArgs::Edit { .. } | ToolArgs::Bash { .. } => {
                let cancel = CancellationToken::new();
                self.cancels.insert(id, cancel.clone());
                self.seams.tasks.spawn(run_tool(
                    Arc::clone(&self.seams.env),
                    self.id,
                    call,
                    id,
                    self.inbox.clone(),
                    cancel,
                    Arc::clone(&self.config),
                ));
            }
            ToolArgs::Wake { at, .. } => {
                // A timer re-arms from its durable deadline after a crash.
                let at = *at;
                let clock = Arc::clone(&self.seams.clock);
                let inbox = self.inbox.clone();
                self.seams.tasks.spawn(async move {
                    let left = at.get().saturating_sub(clock.now().get());
                    clock.sleep(Millis::new(left)).await;
                    let output = measure(ToolOutput::complete("woke".to_owned()));
                    report(
                        &inbox,
                        Event::ToolDone {
                            id,
                            exit: Exit::Ok,
                            output,
                        },
                    )
                    .await;
                });
            }
            ToolArgs::Tasks => {
                let listing = tasks(&self.projector);
                let (inbox, actor) = (self.inbox.clone(), self.id);
                self.seams.tasks.spawn(async move {
                    // It reads the log as a `read` reads a file: a run.
                    barrier(Barrier::Dispatched { actor, id }).await;
                    barrier(Barrier::ToolRan {
                        actor,
                        id,
                        stopped: false,
                    })
                    .await;
                    let output = measure(ToolOutput::complete(listing));
                    report(
                        &inbox,
                        Event::ToolDone {
                            id,
                            exit: Exit::Ok,
                            output,
                        },
                    )
                    .await;
                });
            }
            ToolArgs::Spawn { .. } | ToolArgs::Send { .. } => {
                let work = self.outbound(&call);
                let done = CancellationToken::new();
                if let ToolArgs::Spawn { child, .. } = call.args() {
                    self.spawning.insert(*child, done.clone());
                }
                self.send_out(id, work, None, done);
            }
            ToolArgs::Cancel { .. } => defect("the core does a `cancel` itself"),
        }
        if let Some(after) = deadline {
            self.arm(id, after);
        }
    }

    /// Runs delivery `work` for effect `id` as its own task: once `after`
    /// (a spawn of the same child in flight) is done, between the two
    /// barriers, cancelling `done` once it has landed. A session's loop
    /// never awaits it (P1).
    fn send_out(
        &self,
        id: EffectId,
        work: Outbound,
        after: Option<CancellationToken>,
        done: CancellationToken,
    ) {
        let (registry, actor) = (self.registry.clone(), self.id);
        let inbox = self.inbox.clone();
        self.seams.tasks.spawn(async move {
            if let Some(after) = after {
                after.cancelled().await;
            }
            barrier(Barrier::Delivering { actor, id }).await;
            let (exit, said) = outbox::deliver(&registry, work).await;
            done.cancel();
            barrier(Barrier::Delivered { actor, id }).await;
            let output = measure(ToolOutput::complete(said));
            report(&inbox, Event::ToolDone { id, exit, output }).await;
        });
    }

    /// What a `send` or `spawn` call delivers. The envelope's id is minted
    /// from the call's key, the seq of its durable `Dispatched`, so a rerun
    /// after a crash delivers the same id and lands once (C2).
    fn outbound(&self, call: &ToolCall<Approved, Parsed>) -> Outbound {
        let key = call.key();
        // The planted defect: a rerun mints a fresh id, as if the key were
        // not durable.
        #[cfg(feature = "sim")]
        let key = if self.config.planted == Planted::FreshKeyOnResume {
            key.saturating_add(call.effect().epoch.0.saturating_mul(1 << 32))
        } else {
            key
        };
        let id = EnvelopeId::Minted {
            actor: self.id,
            seq: key,
        };
        let from = Address::Actor(self.id);
        match call.args() {
            ToolArgs::Send { to, text, reply_to } => match to {
                // You read what an actor sends you in its own log.
                Address::User => Outbound::ToUser,
                Address::Actor(recipient) => Outbound::Send {
                    envelope: Envelope {
                        id,
                        from,
                        to: *recipient,
                        reply_to: *reply_to,
                        body: Body::Text(measure(text.clone())),
                    },
                },
                Address::Runtime => defect("the boundary refuses a send to v"),
            },
            ToolArgs::Spawn {
                name,
                tools,
                prompt,
                child,
            } => {
                let depth = self
                    .core
                    .born()
                    .map_or(0, |born| born.depth.saturating_add(1));
                Outbound::Spawn {
                    born: Born {
                        actor: *child,
                        name: name.clone(),
                        parent: Some(self.id),
                        depth,
                        tools: tools.clone(),
                    },
                    prompt: Envelope {
                        id,
                        from,
                        to: *child,
                        reply_to: Some(from),
                        body: Body::Text(measure(prompt.clone())),
                    },
                }
            }
            ToolArgs::Read { .. }
            | ToolArgs::Edit { .. }
            | ToolArgs::Bash { .. }
            | ToolArgs::Wake { .. }
            | ToolArgs::Tasks
            | ToolArgs::Cancel { .. } => defect("only a send or a spawn is delivered"),
        }
    }

    /// The deadline of run `id`: past `after`, v stops waiting and the core
    /// records `Unknown` (`Expired`), unless the run reported first.
    fn arm(&mut self, id: EffectId, after: Millis) {
        // The planted defect: no run ever has a deadline.
        #[cfg(feature = "sim")]
        if self.config.planted == Planted::NoDeadline {
            return;
        }
        let reported = CancellationToken::new();
        self.deadlines.insert(id, reported.clone());
        let clock = Arc::clone(&self.seams.clock);
        let inbox = self.inbox.clone();
        self.seams.tasks.spawn(async move {
            tokio::select! {
                biased;
                () = reported.cancelled() => {}
                () = clock.sleep(after) => report(&inbox, Event::Expired { id }).await,
            }
        });
    }
}

/// The `tasks` tool's listing: every task of the actor, from its live
/// projection.
fn tasks(projector: &Projector) -> String {
    let listing: Vec<String> = projector
        .tasks()
        .map(|task| {
            let what = match &task.subject {
                Some(subject) => format!("{} `{subject}`", task.tool),
                None => task.tool.to_string(),
            };
            match task.end {
                Some(end) => format!("task {}: {what}: ended {end}", task.task_id),
                None => format!("task {}: {what}: running", task.task_id),
            }
        })
        .collect();
    if listing.is_empty() {
        "No tasks.".to_owned()
    } else {
        listing.join("\n")
    }
}

/// A decision the log refused, as the simulation sees it too.
#[cfg_attr(
    not(feature = "sim"),
    expect(
        clippy::missing_const_for_fn,
        reason = "under sim it reports the refusal, which no const fn can"
    )
)]
fn refused(actor: ActorId) -> Persisted {
    #[cfg(feature = "sim")]
    crate::shim::observe(crate::shim::Observation::Refused { actor });
    #[cfg(not(feature = "sim"))]
    let _: ActorId = actor;
    Persisted::Refused
}

/// Answers a delivery or a birth. An asker that went away (a stopping
/// daemon) needs no answer.
fn answer(reply: oneshot::Sender<Outcome>, outcome: Outcome) {
    let _gone: Result<(), Outcome> = reply.send(outcome);
}

/// Sends a completion to the shell. A gone shell means the daemon is
/// stopping; the completion is moot.
async fn report(inbox: &mpsc::Sender<Input>, event: Event<Parsed>) {
    drop(inbox.send(Input::Done(event)).await);
}

async fn run_tool(
    env: Arc<dyn Env>,
    session: ActorId,
    call: ToolCall<Approved, Parsed>,
    id: EffectId,
    inbox: mpsc::Sender<Input>,
    cancel: CancellationToken,
    config: Arc<ShellConfig>,
) {
    barrier(Barrier::Dispatched { actor: session, id }).await;
    #[cfg(feature = "sim")]
    if config.planted == Planted::RunTwice {
        let (_, _) = tool::run(env.as_ref(), session, &call, &cancel, &config.limits).await;
    }
    let (exit, output) = tool::run(env.as_ref(), session, &call, &cancel, &config.limits).await;
    // The planted defect: a run stopped mid-way (`Unknown`) says it never
    // began.
    #[cfg(feature = "sim")]
    let exit = if config.planted == Planted::StoppedAsNotStarted && exit == Exit::Unknown {
        Exit::NotStarted(Unstarted::Cancelled)
    } else {
        exit
    };
    barrier(Barrier::ToolRan {
        actor: session,
        id,
        stopped: matches!(exit, Exit::NotStarted(_)),
    })
    .await;
    let output = measure(output);
    report(&inbox, Event::ToolDone { id, exit, output }).await;
}

/// One model call: stream, batch parts into group commits, then report.
struct ModelTask {
    model: Arc<dyn Model>,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    inbox: mpsc::Sender<Input>,
    config: Arc<ShellConfig>,
    /// The tools the actor is offered: what its calls may name.
    tools: Vec<Tool>,
    id: EffectId,
    cancel: CancellationToken,
}

enum Next {
    Part(Option<ModelPart>),
    Window,
}

/// A summarize call: the model's text, within `summary_bytes`. Tool calls
/// it proposes are ignored; no text at all fails it.
struct SummaryTask {
    actor: ActorId,
    model: Arc<dyn Model>,
    inbox: mpsc::Sender<Input>,
    config: Arc<ShellConfig>,
    id: EffectId,
    cancel: CancellationToken,
}

impl SummaryTask {
    async fn run(self, request: ModelRequest) {
        barrier(Barrier::Summarizing {
            actor: self.actor,
            id: self.id,
        })
        .await;
        let (sender, receiver) = mpsc::channel(self.config.limits.model_parts_queue);
        // Stops the stream when the summary grows too large; an interrupt
        // still stops both.
        let stop = self.cancel.child_token();
        let (streamed, written) = tokio::join!(
            self.model.stream(&request, sender, &stop),
            self.collect(receiver, &stop),
        );
        let summary = match (written, streamed) {
            (Err(TooLarge), _) => Summary::Failed(ModelFault::TooLarge),
            (Ok(_), Err(error)) => Summary::Failed(error.core_fault()),
            (Ok(text), Ok(())) if text.trim().is_empty() => Summary::Failed(ModelFault::Other),
            (Ok(text), Ok(())) => Summary::Written(history::summary(text)),
        };
        report(
            &self.inbox,
            Event::Summarized {
                id: self.id,
                summary,
            },
        )
        .await;
    }

    /// The response's text, its blocks a blank line apart.
    async fn collect(
        &self,
        mut receiver: mpsc::Receiver<ModelPart>,
        stop: &CancellationToken,
    ) -> Result<String, TooLarge> {
        let limit = self.config.limits.summary_bytes;
        let mut text = String::new();
        while let Some(part) = receiver.recv().await {
            let delta = match &part {
                ModelPart::TextStart { .. } if !text.is_empty() => "\n\n",
                ModelPart::TextDelta { delta, .. } => delta.as_str(),
                ModelPart::TextStart { .. }
                | ModelPart::TextEnd { .. }
                | ModelPart::ToolInputStart { .. }
                | ModelPart::ToolInputDelta { .. }
                | ModelPart::ToolInputEnd { .. }
                | ModelPart::ToolCall { .. }
                | ModelPart::Finish { .. } => continue,
            };
            if text.len().saturating_add(delta.len()) > limit {
                stop.cancel();
                while receiver.recv().await.is_some() {}
                return Err(TooLarge);
            }
            text.push_str(delta);
        }
        Ok(text)
    }
}

/// The JSON length of `body`, as a log line holds it.
fn json_len<T: serde::Serialize>(body: &T) -> usize {
    serde_json::to_vec(body).map_or_else(
        |error| defect(&format!("payloads serialize: {error}")),
        |json| json.len(),
    )
}

/// What a JSON array's brackets cost.
const BRACKETS: usize = 2;

/// A response too large to record: a single part, or its tool calls
/// together, would not fit one log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TooLarge;

/// Group commit before the core, by count and by bytes: a batch holds at
/// most `per_batch` parts whose JSON fits `budget` bytes, so its
/// `ModelOutput` record fits a log line; and the response's tool calls
/// together must fit `budget` too, so its `StepFinished` does.
#[derive(Debug)]
struct Batcher {
    budget: usize,
    per_batch: usize,
    batch: Vec<ModelPart>,
    batch_bytes: usize,
    proposals: Vec<Proposal<Parsed>>,
    proposal_bytes: usize,
}

impl Batcher {
    fn new(budget: usize, per_batch: usize) -> Self {
        Self {
            budget,
            per_batch,
            batch: Vec::with_capacity(per_batch),
            batch_bytes: 0,
            proposals: Vec::new(),
            proposal_bytes: 0,
        }
    }

    /// Adds `part`; returns the batches now ready, in order (at most two:
    /// the one `part` did not fit, and a full one).
    ///
    /// # Errors
    /// `part`, or the calls with it, would not fit a log line.
    fn push(
        &mut self,
        part: ModelPart,
        at: &Boundary<'_>,
    ) -> Result<Vec<Vec<ModelPart>>, TooLarge> {
        // A JSON array of n elements is its brackets, the elements and n - 1
        // commas: at most 2 plus each element and a separator.
        let bytes = json_len(&part).saturating_add(1);
        if BRACKETS.saturating_add(bytes) > self.budget {
            return Err(TooLarge);
        }
        if let ModelPart::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } = &part
        {
            let call = proposal(tool_call_id, tool_name, input, at);
            let total = self
                .proposal_bytes
                .saturating_add(json_len(&call).saturating_add(1));
            if BRACKETS.saturating_add(total) > self.budget {
                return Err(TooLarge);
            }
            self.proposal_bytes = total;
            self.proposals.push(call);
        }
        let mut ready = Vec::new();
        if BRACKETS
            .saturating_add(self.batch_bytes)
            .saturating_add(bytes)
            > self.budget
            && let Some(batch) = self.take()
        {
            ready.push(batch);
        }
        self.batch.push(part);
        self.batch_bytes = self.batch_bytes.saturating_add(bytes);
        if self.batch.len() >= self.per_batch
            && let Some(batch) = self.take()
        {
            ready.push(batch);
        }
        Ok(ready)
    }

    /// The batch so far, if any.
    fn take(&mut self) -> Option<Vec<ModelPart>> {
        if self.batch.is_empty() {
            return None;
        }
        self.batch_bytes = 0;
        Some(core::mem::replace(
            &mut self.batch,
            Vec::with_capacity(self.per_batch),
        ))
    }

    /// Whether a batch is waiting for its window.
    const fn holding(&self) -> bool {
        !self.batch.is_empty()
    }
}

impl ModelTask {
    async fn run(self, request: ModelRequest) {
        let (sender, receiver) = mpsc::channel(self.config.limits.model_parts_queue);
        // The batcher stops the stream through its own token when the
        // response is too large to record; an interrupt still stops both.
        let stop = self.cancel.child_token();
        let (streamed, batched) = tokio::join!(
            self.model.stream(&request, sender, &stop),
            self.batch(receiver, &stop),
        );
        let result = match (batched, streamed) {
            (Err(TooLarge), _) => ModelExit::Failed(ModelFault::TooLarge),
            (Ok(proposals), Ok(())) => ModelExit::Finished(proposals),
            (Ok(_), Err(error)) => ModelExit::Failed(error.core_fault()),
        };
        report(
            &self.inbox,
            Event::ModelDone {
                id: self.id,
                result,
            },
        )
        .await;
    }

    async fn flush(&self, batch: Vec<ModelPart>) {
        let parts = measure(batch);
        report(&self.inbox, Event::ModelParts { id: self.id, parts }).await;
    }

    /// Group commit before the core: at most `parts_per_batch` parts within
    /// the record budget, and no part waits longer than `batch_window`.
    /// Returns the proposals of every tool call in the response: the core
    /// records all of them and fails those past its bounds. They are no
    /// more than the parts the response was allowed (`max_parts`), which
    /// the parser already enforced, and fit one record.
    ///
    /// A response too large to record stops the stream (`stop`) and its
    /// remaining parts are read and dropped.
    async fn batch(
        &self,
        mut receiver: mpsc::Receiver<ModelPart>,
        stop: &CancellationToken,
    ) -> Result<Vec<Proposal<Parsed>>, TooLarge> {
        let limits = &self.config.limits;
        let mut batcher = Batcher::new(limits.record_budget(), limits.core.parts_per_batch().get());
        let mut window: Option<Pin<Box<dyn Future<Output = ()> + Send + '_>>> = None;
        loop {
            let next = match window.as_mut() {
                None => Next::Part(receiver.recv().await),
                Some(timer) => tokio::select! {
                    biased;
                    part = receiver.recv() => Next::Part(part),
                    () = timer => Next::Window,
                },
            };
            match next {
                Next::Window => {
                    window = None;
                    if let Some(batch) = batcher.take() {
                        self.flush(batch).await;
                    }
                }
                Next::Part(None) => {
                    if let Some(batch) = batcher.take() {
                        self.flush(batch).await;
                    }
                    return Ok(batcher.proposals);
                }
                Next::Part(Some(part)) => {
                    let boundary = Boundary {
                        tools: &self.tools,
                        text_bytes: limits.wire.max_message_text_bytes,
                        clock: self.clock.as_ref(),
                        entropy: self.entropy.as_ref(),
                    };
                    let Ok(ready) = batcher.push(part, &boundary) else {
                        stop.cancel();
                        while receiver.recv().await.is_some() {}
                        return Err(TooLarge);
                    };
                    for batch in ready {
                        self.flush(batch).await;
                    }
                    if !batcher.holding() {
                        window = None;
                    } else if window.is_none() {
                        window = Some(self.clock.sleep(limits.core.batch_window()));
                    }
                }
            }
        }
    }
}

/// One record's projected chunks, and their length as a JSON array: what
/// they add to a read.
struct Projected {
    chunks: Vec<UiChunk>,
    bytes: usize,
}

impl Projected {
    fn new(chunks: Vec<UiChunk>) -> Self {
        let bytes = encode_chunks(&chunks).len();
        Self { chunks, bytes }
    }
}

/// What `record` and its projected chunks hold in memory, as JSON bytes.
/// A record that does not serialize is a defect: every payload has string
/// keys.
fn held(record: &Record<Parsed>, projected: &Projected) -> usize {
    json_len(record).saturating_add(projected.bytes)
}

/// The verdict the user gave `call` in any turn, if any.
fn verdict_of(records: &[Record<Parsed>], call: &CallId) -> Option<WireVerdict> {
    named(records)
        .filter(|(_, target)| target.is_some_and(|(_, proposal)| proposal.id == *call))
        .filter_map(|(record, _)| match record {
            Record::Approved {
                by: Grant::User, ..
            } => Some(WireVerdict::Grant),
            Record::ToolResult {
                exit: Exit::NotStarted(Unstarted::Denied),
                ..
            } => Some(WireVerdict::Deny),
            Record::Approved { .. }
            | Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::ToolResult { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Dispatched { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. }
            | Record::CompactionStarted { .. }
            | Record::Compacted { .. } => None,
        })
        .last()
}

/// Whether `turn` was asked to stop or ended cancelled.
fn was_cancelled(records: &[Record<Parsed>], turn: TurnId) -> bool {
    records.iter().any(|record| match record {
        Record::CancelRequested { turn: cancelled }
        | Record::TurnEnded {
            turn: cancelled,
            outcome: TurnOutcome::Cancelled,
        } => *cancelled == turn,
        Record::TurnStarted { .. }
        | Record::Born { .. }
        | Record::RulesLoaded { .. }
        | Record::Delivered { .. }
        | Record::TaskEnded { .. }
        | Record::StopSent { .. }
        | Record::StepStarted { .. }
        | Record::ModelOutput { .. }
        | Record::StepReset { .. }
        | Record::StepFinished { .. }
        | Record::Approved { .. }
        | Record::Dispatched { .. }
        | Record::ToolResult { .. }
        | Record::TurnEnded { .. }
        | Record::EpochBumped { .. }
        | Record::CompactionStarted { .. }
        | Record::Compacted { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use verified_domain::id::{CallId, ToolName};

    use super::{Batcher, ModelPart, TooLarge, json_len};
    use crate::payload::{Boundary, ToolInput};
    use crate::seam::{OsEntropy, SystemClock};
    use verified_domain::Tool;

    /// The boundary of an actor offered `read`.
    fn at() -> Boundary<'static> {
        Boundary {
            tools: &[Tool::Read],
            text_bytes: 1_000,
            clock: &SystemClock,
            entropy: &OsEntropy,
        }
    }

    fn delta(text: &str) -> ModelPart {
        ModelPart::TextDelta {
            block: 0,
            delta: text.to_owned(),
        }
    }

    fn call(index: usize) -> ModelPart {
        ModelPart::ToolCall {
            tool_call_id: CallId::parse(&format!("toolu_{index}")).expect("an id"),
            tool_name: ToolName::parse("read").expect("a name"),
            input: ToolInput::parse(r#"{"path":"a.txt"}"#).expect("an object"),
        }
    }

    #[test]
    fn batches_fit_the_record_budget() {
        let part = delta("0123456789");
        let size = json_len(&part) + 1;
        let budget = size * 3 + 2;
        let mut batcher = Batcher::new(budget, 64);
        let mut flushed = Vec::new();
        for _ in 0..7u8 {
            flushed.extend(batcher.push(part.clone(), &at()).expect("fits"));
        }
        flushed.extend(batcher.take());
        assert_eq!(
            flushed.iter().map(Vec::len).collect::<Vec<_>>(),
            [3, 3, 1],
            "split by bytes, nothing lost"
        );
        for batch in &flushed {
            assert!(json_len(batch) <= budget, "a batch fits the budget");
        }
        let mut by_count = Batcher::new(1 << 20, 2);
        assert_eq!(by_count.push(part.clone(), &at()), Ok(Vec::new()));
        assert_eq!(
            by_count.push(part.clone(), &at()),
            Ok(vec![vec![part.clone(), part]])
        );
    }

    #[test]
    fn a_response_too_large_to_record_is_refused() {
        let mut batcher = Batcher::new(64, 64);
        assert_eq!(
            batcher.push(delta(&"x".repeat(100)), &at()),
            Err(TooLarge),
            "one part"
        );
        let one = json_len(&call(0)) + 200;
        let mut calls = Batcher::new(one * 2, 64);
        let mut refused = false;
        for index in 0..16 {
            if calls.push(call(index), &at()).is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "the calls together would not fit one StepFinished");
    }
}
