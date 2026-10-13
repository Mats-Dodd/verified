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
//! 5. start the decision's effects through the [`World`], each of which
//!    reports back into the same inbox.
//!
//! Model parts are batched before they reach the core (group commit:
//! `Limits::parts_per_batch` parts, `ServerLimits::record_budget` bytes or
//! `Limits::batch_window`, on the Clock seam), and every batch reaches the
//! inbox before the call's `ModelDone`. A response whose single part, or
//! whose tool calls together, would not fit one log line fails the call
//! (`Limit::Record`): no record the shell makes is one the log refuses.
//! Effects are cancelled with tokens at safe points, never by dropping them.
//!
//! Should the log refuse an append anyway (its file is full), nothing was
//! written and nothing is applied: the session stops taking records. It
//! interrupts the running turn, whose closing records alone may take the
//! log's reserve ([`ServerLimits::reserve`]), so the turn still ends. It
//! keeps serving its stream and refuses every record from then on; a
//! restart replays the same log and carries on until an append is refused
//! again, ending a turn the crash left running the same way. Nothing
//! acknowledged is ever lost, and the daemon never crash-loops on it.
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
    Approved, Core, Corrupt, Decision, Effect, Event, ModelExit, Outcome,
    Rejection as CoreRejection, ToolCall, Verdict,
};
use verified_domain::id::{CallId, MessageId, SessionId, TurnId};
use verified_domain::{
    Approver, Cause, EffectId, Exit, Fault, Limit, Measured, Proposal, Record, TurnOutcome,
};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{
    Approve, ApproveReply, Cancel, CancelReply, Outcome as WireOutcome, Rejection, SendMessage,
    SendMessageReply, Verdict as WireVerdict,
};
use verified_wire::stream::{Freshness, Offset, ReadFrom, encode_chunks};

use crate::defect;
use crate::env::Env;
use crate::history;
use crate::limits::ServerLimits;
use crate::log::{Log, LogError};
use crate::model::Model;
use crate::payload::{ModelPart, Parsed, ToolOutput, Truncation, measure, proposal};
use crate::projection::Projector;
use crate::seam::{Clock, Entropy, World, id_entropy};
use crate::shim::{Barrier, barrier};
use crate::tool;

/// Everything nondeterministic a session uses.
#[derive(Debug, Clone)]
pub struct Seams {
    pub model: Arc<dyn Model>,
    pub env: Arc<dyn Env>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    pub world: World,
}

/// Configuration every session shares: data from main.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub limits: ServerLimits,
    /// The model's name, as the stream's `data-turn-start` reports it.
    pub model: String,
    /// The system prompt.
    pub system: String,
    /// Deliberate defects a simulation plants to show its checks catch
    /// them. Empty without the `sim` feature.
    pub planted: Planted,
}

/// Deliberate defects, for the simulation's mutation tests. Without the
/// `sim` feature there are none: the shipped daemon cannot plant one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Planted {
    /// Start tool runs before their decision is durable (breaks Law 2:
    /// persist before acting). The world check I1 must catch it.
    #[cfg(feature = "sim")]
    pub run_before_persist: bool,
    /// Run each tool twice, reporting once (breaks at-most-once inside one
    /// dispatch). The model's replay must catch the second act.
    #[cfg(feature = "sim")]
    pub run_twice: bool,
    /// Persist an `Unknown` result as `Interrupted` (breaks U4 in the shell,
    /// below the core). The replay must catch the durable records differing
    /// from the decided ones.
    #[cfg(feature = "sim")]
    pub unknown_as_interrupted: bool,
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
    Read {
        from: ReadFrom,
        reply: oneshot::Sender<StreamRead>,
    },
    Records {
        reply: oneshot::Sender<Vec<Record<Parsed>>>,
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
pub struct Tail {
    shared: Arc<TailState>,
}

struct TailState {
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

impl fmt::Debug for Tail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tail")
            .field("next", &self.get())
            .finish_non_exhaustive()
    }
}

impl Tail {
    #[expect(
        clippy::disallowed_types,
        reason = "the atomics described on TailState"
    )]
    fn new(next: u64) -> (Self, TailWriter) {
        let tail = Self {
            shared: Arc::new(TailState {
                next: core::sync::atomic::AtomicU64::new(next),
                open: core::sync::atomic::AtomicBool::new(true),
                notify: Notify::new(),
            }),
        };
        (tail.clone(), TailWriter { tail })
    }

    /// The seq the next record will get.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.shared.next.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Waits until the tail is past `seq` (a record at `seq` is committed),
    /// and returns it.
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

/// The session's side of its [`Tail`]; dropping it tells followers the
/// session stopped.
struct TailWriter {
    tail: Tail,
}

impl TailWriter {
    fn set(&self, next: u64) {
        self.tail
            .shared
            .next
            .store(next, core::sync::atomic::Ordering::Release);
        self.tail.shared.notify.notify_waiters();
    }
}

impl Drop for TailWriter {
    fn drop(&mut self) {
        self.tail
            .shared
            .open
            .store(false, core::sync::atomic::Ordering::Release);
        self.tail.shared.notify.notify_waiters();
    }
}

/// How the rest of the daemon talks to one session.
#[derive(Clone)]
pub struct SessionHandle {
    session: SessionId,
    inbox: mpsc::Sender<Input>,
    tail: Tail,
    /// One permit per live stream follower (`sse_followers_per_session`).
    followers: Arc<Semaphore>,
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
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

    /// Chunks from `from` to the end of the stream (bounded per read).
    ///
    /// # Errors
    /// The session is gone.
    pub async fn read(&self, from: ReadFrom) -> Result<StreamRead, Unavailable> {
        self.ask(|reply| Input::Read { from, reply }).await
    }

    /// Every committed record, for world-level checkers.
    ///
    /// # Errors
    /// The session is gone.
    pub async fn records(&self) -> Result<Vec<Record<Parsed>>, Unavailable> {
        self.ask(|reply| Input::Records { reply }).await
    }

    /// The seq the next record will get; changes after every commit.
    #[must_use]
    pub fn tail(&self) -> Tail {
        self.tail.clone()
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
    id: SessionId,
    core: Core<Parsed>,
    log: Box<dyn Log>,
    records: Vec<Record<Parsed>>,
    chunks: Vec<Projected>,
    projector: Projector,
    seams: Seams,
    config: Arc<ShellConfig>,
    inbox: mpsc::Sender<Input>,
    tail: TailWriter,
    cancels: BTreeMap<EffectId, CancellationToken>,
    /// JSON bytes of `records` and `chunks`, against `session_bytes`.
    held_bytes: usize,
    /// Why the log stopped taking records, once it has: from then on only
    /// the running turn's closing records are persisted, into the reserve.
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

/// The task that runs a session; spawn it through the world.
pub type Running = Pin<Box<dyn Future<Output = Result<(), ShellError>> + Send>>;

impl Session {
    /// Recovers a session from its log's records and returns its handle and
    /// the task to run. The task first resumes (bumping the epoch and
    /// restarting or resolving whatever was in flight).
    ///
    /// # Errors
    /// The records do not replay.
    pub fn start(
        session: SessionId,
        log: Box<dyn Log>,
        records: Vec<Record<Parsed>>,
        seams: Seams,
        config: Arc<ShellConfig>,
    ) -> Result<(SessionHandle, Running), ShellError> {
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
        let (inbox, receiver) = mpsc::channel(config.limits.session_inbox);
        let (tail_receiver, tail) = Tail::new(core.seq());
        let handle = SessionHandle {
            session,
            inbox: inbox.clone(),
            tail: tail_receiver,
            followers: Arc::new(Semaphore::new(config.limits.sse_followers_per_session)),
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
            tail,
            cancels: BTreeMap::new(),
            held_bytes,
            full: None,
        };
        Ok((handle, Box::pin(shell.run(receiver))))
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<Input>) -> Result<(), ShellError> {
        let mut resumed = self.resume().await?;
        if matches!(resumed, Persisted::Refused) && self.core.running().is_some() {
            // The log has no room to carry the turn on: interrupt it, and
            // resume again, which now ends it.
            self.close().await?;
            resumed = self.resume().await?;
        }
        if let Persisted::Done(_, effects) = resumed {
            self.start_effects(effects).await;
        }
        let shutdown = self.seams.world.shutdown().clone();
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
        let resumed = self.core.resume();
        #[cfg(feature = "sim")]
        crate::shim::observe(crate::shim::Observation::Resumed {
            records: resumed.records.clone(),
        });
        self.persist(resumed).await
    }

    /// Steps 2 and 3: durable, then applied and projected. An ordinary
    /// append leaves the log's reserve free. Once the log has refused one,
    /// only the running turn's closing records are persisted, into the
    /// reserve.
    async fn persist(&mut self, decision: Decision<Parsed>) -> Result<Persisted, ShellError> {
        let Decision {
            records,
            effects,
            outcome,
        } = decision;
        #[cfg(feature = "sim")]
        let records = if self.config.planted.unknown_as_interrupted {
            unknown_as_interrupted(records)
        } else {
            records
        };
        #[cfg(feature = "sim")]
        let effects = if self.config.planted.run_before_persist {
            self.run_before_persist(effects).await
        } else {
            effects
        };
        if !records.is_empty() {
            let spare = if self.full.is_none() {
                self.config.limits.reserve()
            } else if self.core.running().is_some() && records.iter().all(closes) {
                0
            } else {
                return Ok(Persisted::Refused);
            };
            assert!(
                self.log.next_seq() == self.core.seq(),
                "the log and the core agree on the next seq"
            );
            match self.log.append(&records, spare).await {
                Ok(()) => {}
                Err(error) if error.refused() => {
                    self.full.get_or_insert_with(|| error.to_string());
                    return Ok(Persisted::Refused);
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
            self.tail.set(self.core.seq());
        }
        Ok(Persisted::Done(outcome, effects))
    }

    /// The planted defect: starts the decision's tool runs at once, before
    /// its records are durable, and returns the other effects.
    #[cfg(feature = "sim")]
    async fn run_before_persist(&mut self, effects: Vec<Effect<Parsed>>) -> Vec<Effect<Parsed>> {
        let mut rest = Vec::new();
        for effect in effects {
            match effect {
                Effect::RunTool { .. } => self.start_effect(effect).await,
                Effect::CallModel { .. }
                | Effect::CancelModel { .. }
                | Effect::AskApproval { .. }
                | Effect::CancelTool { .. }
                | Effect::Sleep { .. } => rest.push(effect),
            }
        }
        rest
    }

    async fn decide(&mut self, event: Event<Parsed>) -> Result<Persisted, ShellError> {
        #[cfg(feature = "sim")]
        let observed = event.clone();
        let decision = self.core.decide(event);
        #[cfg(feature = "sim")]
        crate::shim::observe(crate::shim::Observation::Decided {
            event: observed,
            records: decision.records.clone(),
        });
        let persisted = self.persist(decision).await?;
        if matches!(persisted, Persisted::Refused) {
            self.close().await?;
        }
        Ok(persisted)
    }

    /// Once the log is full, interrupts the running turn (if it is not
    /// stopping already), so that its closing records end it.
    async fn close(&mut self) -> Result<(), ShellError> {
        let Some(turn) = self.core.running() else {
            return Ok(());
        };
        let decision = self.core.decide(Event::Interrupt { turn });
        #[cfg(feature = "sim")]
        crate::shim::observe(crate::shim::Observation::Decided {
            event: Event::Interrupt { turn },
            records: decision.records.clone(),
        });
        if let Persisted::Done(_, effects) = self.persist(decision).await? {
            self.start_effects(effects).await;
        }
        Ok(())
    }

    /// The answer to every command once the log is full.
    fn full(&self) -> Rejection {
        Rejection::SessionFull {
            session_id: self.id,
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
                    Event::ModelDone { id, .. } | Event::ToolDone { id, .. } => {
                        self.cancels.remove(id);
                    }
                    Event::Message { .. }
                    | Event::Approve { .. }
                    | Event::Interrupt { .. }
                    | Event::ModelParts { .. }
                    | Event::Woke { .. } => {}
                }
                let closed = closing(&event);
                let persisted = if self.full.is_none() {
                    self.decide(event).await?
                } else {
                    Persisted::Refused
                };
                let persisted = match (persisted, closed) {
                    // The log is full: the completion as it takes one.
                    (Persisted::Refused, Some(closed)) => self.decide(closed).await?,
                    (persisted, _) => persisted,
                };
                if let Persisted::Done(_, effects) = persisted {
                    self.start_effects(effects).await;
                }
            }
            Input::Message { command, reply } => {
                if self.held_bytes >= self.config.limits.session_bytes
                    && self.turn_of(command.message_id).is_none()
                {
                    drop(reply.send(Err(Rejection::SessionFull {
                        session_id: self.id,
                        reason: format!(
                            "it holds {} bytes, its limit is {}",
                            self.held_bytes, self.config.limits.session_bytes
                        ),
                    })));
                    return Ok(());
                }
                let turn = TurnId::from_parts(
                    self.seams.clock.now(),
                    &id_entropy(self.seams.entropy.as_ref()),
                );
                let text = command.text.as_str().to_owned();
                let event = Event::Message {
                    message: command.message_id,
                    turn,
                    text: measure(text.clone()),
                };
                match self.decide(event).await? {
                    Persisted::Done(outcome, effects) => {
                        let answer = self.message_answer(outcome, &command, turn, &text);
                        drop(reply.send(answer));
                        self.start_effects(effects).await;
                    }
                    Persisted::Refused => drop(reply.send(Err(self.full()))),
                }
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
                        self.start_effects(effects).await;
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
                        self.start_effects(effects).await;
                    }
                    Persisted::Refused => drop(reply.send(Err(self.full()))),
                }
            }
            Input::Read { from, reply } => drop(reply.send(self.read(from))),
            Input::Records { reply } => drop(reply.send(self.records.clone())),
        }
        Ok(())
    }

    fn message_answer(
        &self,
        outcome: Outcome,
        command: &SendMessage,
        turn: TurnId,
        text: &str,
    ) -> Result<SendMessageReply, Rejection> {
        match outcome {
            Outcome::Accepted => Ok(SendMessageReply {
                turn_id: turn,
                outcome: WireOutcome::Created,
            }),
            Outcome::Exists => {
                let Some((turn, original)) = self.turn_of(command.message_id) else {
                    defect("the core saw a message the log does not hold");
                };
                if original.body == text {
                    Ok(SendMessageReply {
                        turn_id: turn,
                        outcome: WireOutcome::Exists,
                    })
                } else {
                    Err(Rejection::MessageExistsWithDifferentText {
                        message_id: command.message_id,
                    })
                }
            }
            Outcome::Rejected(CoreRejection::Busy) => {
                let running = self
                    .core
                    .running()
                    .unwrap_or_else(|| defect("busy without a running turn"));
                Err(Rejection::SessionBusy {
                    session_id: self.id,
                    turn_id: running,
                })
            }
            Outcome::Rejected(CoreRejection::SessionFull) => Err(Rejection::SessionFull {
                session_id: self.id,
                reason: format!(
                    "it holds its limit of {} turns",
                    self.config.limits.core.turns_per_session()
                ),
            }),
            Outcome::Rejected(CoreRejection::UnknownCall | CoreRejection::NotRunning)
            | Outcome::Stale => defect("a message is accepted, a repeat, busy or full"),
        }
    }

    /// The turn `message` started, and its text, if the log holds it.
    fn turn_of(&self, message: MessageId) -> Option<(TurnId, &Measured<String>)> {
        self.records.iter().find_map(|record| match record {
            Record::TurnStarted {
                turn,
                message: seen,
                text,
            } if *seen == message => Some((*turn, text)),
            Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        })
    }

    fn approve_answer(
        &self,
        outcome: Outcome,
        command: &Approve,
    ) -> Result<ApproveReply, Rejection> {
        match outcome {
            Outcome::Accepted => Ok(ApproveReply {
                outcome: WireOutcome::Created,
            }),
            Outcome::Exists | Outcome::Rejected(CoreRejection::UnknownCall) => {
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
            Outcome::Rejected(
                CoreRejection::Busy | CoreRejection::SessionFull | CoreRejection::NotRunning,
            )
            | Outcome::Stale => defect("a verdict is accepted, a repeat or for an unknown call"),
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
            Outcome::Rejected(
                CoreRejection::Busy | CoreRejection::SessionFull | CoreRejection::UnknownCall,
            )
            | Outcome::Stale => defect("an interrupt is accepted, a repeat or not running"),
        }
    }

    fn read(&self, from: ReadFrom) -> StreamRead {
        let tail = self.core.seq();
        let start = match from {
            ReadFrom::Now => tail,
            ReadFrom::Offset(offset) => offset.seq().min(tail),
        };
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
    async fn start_effects(&mut self, effects: Vec<Effect<Parsed>>) {
        for effect in effects {
            self.start_effect(effect).await;
        }
    }

    async fn start_effect(&mut self, effect: Effect<Parsed>) {
        match effect {
            Effect::CallModel { id, .. } => {
                let request = history::request(
                    &self.records,
                    &self.config.system,
                    self.config.limits.model_max_tokens,
                );
                let cancel = CancellationToken::new();
                self.cancels.insert(id, cancel.clone());
                let task = ModelTask {
                    model: Arc::clone(&self.seams.model),
                    clock: Arc::clone(&self.seams.clock),
                    inbox: self.inbox.clone(),
                    config: Arc::clone(&self.config),
                    id,
                    cancel,
                };
                self.seams.world.spawn(task.run(request));
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
            Effect::RunTool { call } => {
                let id = call.effect();
                barrier(Barrier::Dispatched {
                    epoch: id.epoch.0,
                    seq: id.seq,
                })
                .await;
                let cancel = CancellationToken::new();
                self.cancels.insert(id, cancel.clone());
                self.seams.world.spawn(run_tool(
                    Arc::clone(&self.seams.env),
                    self.id,
                    call,
                    id,
                    self.inbox.clone(),
                    cancel,
                    Arc::clone(&self.config),
                ));
            }
            Effect::Sleep { id, delay } => {
                let clock = Arc::clone(&self.seams.clock);
                let inbox = self.inbox.clone();
                self.seams.world.spawn(async move {
                    clock.sleep(delay).await;
                    report(&inbox, Event::Woke { id }).await;
                });
            }
        }
    }
}

/// Whether `record` is one that ends a turn, and so may take the log's
/// reserve: a short line with no payload.
const fn closes(record: &Record<Parsed>) -> bool {
    match record {
        Record::CancelRequested { .. }
        | Record::ToolResult { .. }
        | Record::TurnEnded { .. }
        | Record::EpochBumped { .. } => true,
        Record::TurnStarted { .. }
        | Record::StepStarted { .. }
        | Record::ModelOutput { .. }
        | Record::StepReset { .. }
        | Record::StepFinished { .. }
        | Record::Approved { .. }
        | Record::Dispatched { .. } => false,
    }
}

/// A completion as a full log takes it, so the core answers with closing
/// records that fit the reserve: a tool's output is dropped, and a model's
/// finished response (its calls would be a `StepFinished`) counts as
/// interrupted, as the turn being stopped is. `None` for an event a full
/// log drops: streamed parts, and a timer, which the turn's interrupt made
/// stale.
fn closing(event: &Event<Parsed>) -> Option<Event<Parsed>> {
    match event {
        Event::ToolDone { id, exit, output } => {
            let shown = u64::try_from(output.body.text.len()).unwrap_or(u64::MAX);
            let omitted_bytes = match output.body.truncation {
                Truncation::Complete => shown,
                Truncation::Truncated { omitted_bytes } => omitted_bytes.saturating_add(shown),
            };
            Some(Event::ToolDone {
                id: *id,
                exit: *exit,
                output: measure(ToolOutput {
                    text: String::new(),
                    truncation: Truncation::Truncated { omitted_bytes },
                }),
            })
        }
        Event::ModelDone { id, result } => Some(Event::ModelDone {
            id: *id,
            result: match result {
                ModelExit::Finished(_) => ModelExit::Failed(Cause::Interrupted),
                ModelExit::Failed(cause) => ModelExit::Failed(*cause),
            },
        }),
        Event::Message { .. }
        | Event::Approve { .. }
        | Event::Interrupt { .. }
        | Event::ModelParts { .. }
        | Event::Woke { .. } => None,
    }
}

/// The planted defect: `records` with every `Unknown` result made
/// `Interrupted`, as if the shell knew the call never ran.
#[cfg(feature = "sim")]
fn unknown_as_interrupted(mut records: Vec<Record<Parsed>>) -> Vec<Record<Parsed>> {
    for record in &mut records {
        if let Record::ToolResult { exit, .. } = record
            && *exit == Exit::Err(Cause::Unknown)
        {
            *exit = Exit::Err(Cause::Interrupted);
        }
    }
    records
}

/// Sends a completion to the shell. A gone shell means the daemon is
/// stopping; the completion is moot.
async fn report(inbox: &mpsc::Sender<Input>, event: Event<Parsed>) {
    drop(inbox.send(Input::Done(event)).await);
}

async fn run_tool(
    env: Arc<dyn Env>,
    session: SessionId,
    call: ToolCall<Approved, Parsed>,
    id: EffectId,
    inbox: mpsc::Sender<Input>,
    cancel: CancellationToken,
    config: Arc<ShellConfig>,
) {
    #[cfg(feature = "sim")]
    if config.planted.run_twice {
        let (_, _) = tool::run(env.as_ref(), session, &call, &cancel, &config.limits).await;
    }
    let (exit, output) = tool::run(env.as_ref(), session, &call, &cancel, &config.limits).await;
    barrier(Barrier::ToolRan {
        epoch: id.epoch.0,
        seq: id.seq,
    })
    .await;
    let output = measure(output);
    report(&inbox, Event::ToolDone { id, exit, output }).await;
}

/// One model call: stream, batch parts into group commits, then report.
struct ModelTask {
    model: Arc<dyn Model>,
    clock: Arc<dyn Clock>,
    inbox: mpsc::Sender<Input>,
    config: Arc<ShellConfig>,
    id: EffectId,
    cancel: CancellationToken,
}

enum Next {
    Part(Option<ModelPart>),
    Window,
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
    fn push(&mut self, part: ModelPart) -> Result<Vec<Vec<ModelPart>>, TooLarge> {
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
            let call = proposal(tool_call_id, tool_name, input);
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
    async fn run(self, request: crate::model::ModelRequest) {
        let (sender, receiver) = mpsc::channel(self.config.limits.model_parts_queue);
        // The batcher stops the stream through its own token when the
        // response is too large to record; an interrupt still stops both.
        let stop = self.cancel.child_token();
        let (streamed, batched) = tokio::join!(
            self.model.stream(&request, sender, &stop),
            self.batch(receiver, &stop),
        );
        let result = match (batched, streamed) {
            (Err(TooLarge), _) => ModelExit::Failed(Cause::Failed(Fault::Limit(Limit::Record))),
            (Ok(proposals), Ok(())) => ModelExit::Finished(proposals),
            (Ok(_), Err(error)) => ModelExit::Failed(error.core_cause()),
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
                    let held = batcher.holding();
                    let Ok(ready) = batcher.push(part) else {
                        stop.cancel();
                        while receiver.recv().await.is_some() {}
                        return Err(TooLarge);
                    };
                    for batch in ready {
                        self.flush(batch).await;
                    }
                    if !batcher.holding() {
                        window = None;
                    } else if !held || window.is_none() {
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
    let mut calls: Vec<&CallId> = Vec::new();
    let mut verdict = None;
    let named = |calls: &[&CallId], index: usize| calls.get(index).is_some_and(|id| *id == call);
    for record in records {
        match record {
            Record::TurnStarted { .. } => calls.clear(),
            Record::StepFinished {
                calls: proposed, ..
            } => {
                calls.extend(proposed.iter().map(|proposal| &proposal.id));
            }
            Record::Approved {
                call: index,
                by: Approver::User,
            } if named(&calls, index.0) => verdict = Some(WireVerdict::Grant),
            Record::ToolResult {
                call: index,
                exit: Exit::Err(Cause::Denied),
                ..
            } if named(&calls, index.0) => verdict = Some(WireVerdict::Deny),
            Record::Approved { .. }
            | Record::ToolResult { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::Dispatched { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => {}
        }
    }
    verdict
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
        | Record::StepStarted { .. }
        | Record::ModelOutput { .. }
        | Record::StepReset { .. }
        | Record::StepFinished { .. }
        | Record::Approved { .. }
        | Record::Dispatched { .. }
        | Record::ToolResult { .. }
        | Record::TurnEnded { .. }
        | Record::EpochBumped { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use verified_domain::id::{CallId, ToolName};

    use super::{Batcher, ModelPart, TooLarge, json_len};
    use crate::payload::ToolInput;

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
            flushed.extend(batcher.push(part.clone()).expect("fits"));
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
        assert_eq!(by_count.push(part.clone()), Ok(Vec::new()));
        assert_eq!(
            by_count.push(part.clone()),
            Ok(vec![vec![part.clone(), part]])
        );
    }

    #[test]
    fn a_response_too_large_to_record_is_refused() {
        let mut batcher = Batcher::new(64, 64);
        assert_eq!(
            batcher.push(delta(&"x".repeat(100))),
            Err(TooLarge),
            "one part"
        );
        let one = json_len(&call(0)) + 200;
        let mut calls = Batcher::new(one * 2, 64);
        let mut refused = false;
        for index in 0..16 {
            if calls.push(call(index)).is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "the calls together would not fit one StepFinished");
    }
}
