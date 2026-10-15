//! `Core<P>` with `decide`, `apply`, `recover` and `resume`.
//!
//! All durable state is a fold of `apply` over the log. There is one
//! transition function: `spec_apply` says what a record does to the
//! abstract state (or that the record cannot follow it), `apply` implements
//! exactly that, and `decide` never keeps a model of its own. It plans on a
//! scratch copy of the core, applying each record it emits to the copy and
//! reading the copy to choose the next, so what it decides is what `apply`
//! will do. Effect ids are derived from the log (`EffectId { epoch, seq }`
//! with `seq` the licensing record), so the in-flight table is a function of
//! durable state and a completion matches only while its licensing record
//! is still the live one.
//!
//! Proved here (`just verify`):
//!
//! - `recover(log)` succeeds exactly when the spec fold `replay(log)` does,
//!   and then equals it.
//! - every state `apply` reaches keeps the call ledger invariant: each call
//!   is approved at most once, dispatched at most once and only after it was
//!   approved, and only a valid request is ever approved.
//! - every grant is the user's command or a rule in force that fits the
//!   call (C1, `grant_fits`): `apply` refuses any other `Approved`, and
//!   `decide` grants by the first fitting rule (`rules::grant`) or asks. A
//!   user's grant is recorded only on the user's approval command, as its
//!   decision's first record (`honest`); `resume` records none.
//! - an envelope is delivered at most once (C2, T4): a `Delivered` for an id
//!   already seen does not apply.
//! - `decide` and `resume` return records that apply, one after another,
//!   to the state they were given (never a refusal), and every effect they
//!   start is licensed (C1, `runs_ok`): a `RunTool` by its call's
//!   `Dispatched` or by a running task started by that very call, and a
//!   `Stop` by a running stop task's `StopSent`, the very record its effect
//!   id names, in the current epoch.
//!   For `decide` that record is one of the decision's own (`decided`):
//!   nothing is started twice by later decisions. Only `resume` may start
//!   again what was started before it, and never an `Unsafe` call or task,
//!   in the new epoch (`resumed`, C2), nor anything under a durable cancel
//!   or stop (C3: a call reruns only while its turn proceeds, and a stopped
//!   actor reruns only its stops).
//! - a turn the user asked to stop can only end `Cancelled` (`apply` refuses
//!   anything else), so `decide` never ends one otherwise.
//! - a result claims no more than v knows (T3, `result_fits`, `end_fits`):
//!   one the core synthesizes is `NotStarted` only for a call never
//!   dispatched (or a timer that never fired) and `Unknown` for one that
//!   may have acted, besides what the core itself does at dispatch (a
//!   background call's start, a `cancel`); a run reports only once
//!   dispatched, and its report is recorded exactly as it came, only on
//!   its `ToolDone` (`honest`).
//! - cancel is revocation (C3): once a turn's cancel is durable, no step,
//!   retry, approval or dispatch applies until the turn ends
//!   (`lemma_nothing_starts_after_a_cancel`); once a stop is durable, no
//!   turn, step, approval, dispatch or delivery applies, ever
//!   (`lemma_nothing_starts_after_a_stop`). The stop cascade's `StopSent`
//!   and their `Stop` effects are the only things a stopped actor starts.
//! - a known outcome is never overwritten, and a task ends at most once
//!   (T3): no record gives a resolved call another result
//!   (`lemma_known_outcome_stays`) or ends an ended task
//!   (`lemma_a_task_ends_once`), so of a run's report and its deadline
//!   (`Expired`), the first to reach the core wins.
//! - the mailbox (T4): every envelope the actor held, carried or still
//!   waiting, appears once (`actor_ok`, kept by `lemma_actor_keeps`), so a
//!   model call never carries one twice and a task's notification lands
//!   once; a step carries exactly the batch at the head of the mailbox, and
//!   only at a step boundary, once every call of the step before resolved
//!   (`lemma_carried_at_boundaries`, T4).
//! - model history compaction: a cut is always now, at a beat, with every
//!   call resolved (`lemma_cut_is_now`); neither of its records touches the
//!   mailbox or what was carried, so unread mail is never inside a cut
//!   (`lemma_cut_keeps_mail`, T4); and a summarize call starts only once
//!   its own `CompactionStarted`, the live compaction, is durable
//!   (`summarize_ok`, T1).
//!
//! Model-checked or tested, not proved (Stateright, `verified-model`, and
//! the scenario tests in `tests/turns.rs` and `tests/actors.rs`):
//!
//! - the core has no concurrency rules: every approved call is dispatched
//!   at once, in proposal order, beside whatever already runs.
//! - after a crash, a dispatched `Unsafe` call without a result is resolved
//!   `Unknown`, never rerun or reported otherwise.
//! - no turn waits on another actor or on a task (P1): a background call
//!   resolves `Ok` in the decision that dispatches it, and a `send` or
//!   `spawn` on its delivery, never on another actor's turn.
//! - the limits (P1): `decide` delivers no envelope past
//!   `messages_per_actor` (a stop always lands), starts no turn past
//!   `turns_per_actor`, no task past `tasks_per_actor`, and no spawn past
//!   `depth`.
//! - steering: while a user's message waits, no call of the step waits for
//!   the user: each is denied (`NotStarted(Steered)`), however the message
//!   and the step met (during the model call, during the wait, or across a
//!   crash); notifications and agents' messages that arrive together are
//!   carried together.
//! - a run is signalled at most once: Esc signals its turn's runs, a
//!   `cancel` its task (`Task::cancelled`), and a later stop or deadline
//!   signals neither again.

#![expect(
    clippy::indexing_slicing,
    reason = "Verus proves every index in this module is in bounds"
)]
#![expect(
    clippy::arithmetic_side_effects,
    reason = "Verus proves no arithmetic in this module overflows"
)]
#![expect(
    clippy::unreachable,
    reason = "vstd's `Ghost(..)` erases to `unreachable!()` in plain builds; ghost code never runs"
)]
#![expect(clippy::len_zero, reason = "vstd specifies `len`, not `is_empty`")]

use core::fmt;

use vstd::prelude::*;

#[cfg(verus_only)]
use verified_domain::record::class_of;
use verified_domain::rules::RuleView;
use verified_domain::shape::ShapeView;
use verified_domain::{
    ActorId, ActorName, Address, Body, Born, CallId, CallIx, Class, EffectId, Envelope, EnvelopeId,
    Epoch, Exit, Failure, Grant, Limit, Limits, Measured, Millis, ModelFault, OPEN_ENTRY_BYTES,
    OpenWork, Payload, Proposal, Record, Request, Reset, Rules, Shape, StepId, TaskId, Tool,
    TurnId, TurnOutcome, Unstarted,
};

use crate::effect::{self, Decision, Effect, Outcome, Rejection};
#[cfg(verus_only)]
use crate::effect::{Approved, ToolCall};
use crate::event::{Event, Loaded, ModelExit, Summary, Verdict};
use crate::mailbox::{self, Waiting};
#[cfg(verus_only)]
use crate::mailbox::{held, ids_of, is_batch};
use crate::rules;
#[cfg(verus_only)]
use crate::rules::fits;

verus! {

// ---------------------------------------------------------------------------
// State shared by the executable core and its specification.
// ---------------------------------------------------------------------------

/// A call's lifecycle. `Dispatched` keeps the licensing record's sequence
/// number: the `RunTool` effect id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Phase {
    Proposed,
    Approved,
    Dispatched { seq: u64 },
    Resolved,
}

/// Where the running turn is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Stage {
    /// `TurnStarted` applied; the first `StepStarted` is next.
    Starting,
    /// A model call licensed by record `since` is in flight.
    Model { since: u64 },
    /// Waiting before a retry (licensed by the `StepReset` at `since`).
    Backoff { since: u64, reset: Reset },
    /// The model finished the step; its calls are being resolved.
    Tools,
    /// The model's history is being compacted at a beat, its cut the
    /// `CompactionStarted` at `since`; `Compacted` returns to `then`.
    Compacting { since: u64, then: Then },
}

/// The beat a compaction inside a turn returns to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Then {
    /// The turn's first step is next.
    Starting,
    /// The next step after the calls of the last one.
    Tools,
}

/// The stage `then` names.
pub open spec fn spec_stage(then: Then) -> Stage {
    match then {
        Then::Starting => Stage::Starting,
        Then::Tools => Stage::Tools,
    }
}

impl Then {
    const fn stage(self) -> (stage: Stage)
        ensures
            stage == spec_stage(self),
    {
        match self {
            Then::Starting => Stage::Starting,
            Then::Tools => Stage::Tools,
        }
    }
}

/// Whether the user asked the running turn to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Intent {
    Proceed,
    Cancel,
}

/// The running turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Open {
    pub turn: TurnId,
    pub step: StepId,
    /// Retries of the current step spent from `Limits::retry_schedule`.
    pub retries: usize,
    pub stage: Stage,
    pub intent: Intent,
    /// Index of the current step's first call.
    pub first_call: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Turn {
    Idle,
    Open(Open),
}

// ---------------------------------------------------------------------------
// Specification: the abstract state, what one record does to it, and the
// fold over a log.
// ---------------------------------------------------------------------------

/// What started a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Origin {
    /// A background call (`bash` in the background, `wake`): its index in
    /// its turn, so the task runs exactly that call's request.
    Call { call: CallIx },
    /// A `StopSent`: the stop of a child.
    Stop { child: ActorId },
}

/// A task: an effect that runs on beside the actor's turns, and ends once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Task {
    /// The seq of the record that started it: its `TaskId`, and its
    /// effect's.
    pub seq: u64,
    pub class: Class,
    pub origin: Origin,
    pub ended: bool,
    /// A cancel of it was dispatched: it has been signalled (C3), and is
    /// signalled at most once.
    pub cancelled: bool,
}

/// One call as the spec sees it, with how many `Approved` and `Dispatched`
/// records were applied to it.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `nat` values that exist only while verifying")]
pub struct CallView {
    pub phase: Phase,
    /// The class of the call's tool; `None` for a request that did not
    /// parse into a known tool's arguments.
    pub class: Option<Class>,
    /// The call's shape; `None` for an invalid request.
    pub shape: Option<ShapeView>,
    pub approvals: nat,
    pub dispatches: nat,
}

/// The abstract state the proofs talk about.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `Seq` values that exist only while verifying")]
pub struct CoreView {
    pub epoch: u64,
    pub seq: u64,
    /// This actor's name, once it is born.
    pub name: Option<Seq<u8>>,
    /// This actor's address, once it is born.
    pub actor: Option<ActorId>,
    /// Its spawn depth (0 until it is born).
    pub depth: u32,
    /// A stop was delivered: nothing starts any more (C3).
    pub stopped: bool,
    /// The rules in force: the seq of their `RulesLoaded` record, and them.
    pub rules: Option<(u64, Seq<RuleView>)>,
    /// Every envelope delivered, for deduplication.
    pub envelopes: Seq<EnvelopeId>,
    /// Envelopes delivered or notifications, not yet carried, in arrival
    /// order.
    pub mailbox: Seq<Waiting>,
    /// Every envelope a model call carried, in order (T4).
    pub consumed: Seq<EnvelopeId>,
    /// Turns started.
    pub turns: nat,
    pub turn: Turn,
    pub calls: Seq<CallView>,
    /// Every task started, in order.
    pub tasks: Seq<Task>,
    /// Every actor a dispatched `spawn` names: the stop cascade's targets.
    pub children: Seq<ActorId>,
    /// The seq of the latest `CompactionStarted` between turns, until its
    /// `Compacted` or a turn supersedes it. Its summarize call may be in
    /// flight, failed (a failure records nothing) or cut short by a crash:
    /// the log cannot tell, so `resume` compacts again, and a stop or a
    /// turn cancels it.
    pub compacting: Option<u64>,
}

/// Approve before dispatch, at most once each, and only a valid request.
pub open spec fn call_ok(call: CallView) -> bool {
    &&& call.approvals <= 1
    &&& call.dispatches <= call.approvals
    &&& (call.phase is Proposed ==> call.approvals == 0)
    &&& (call.phase is Approved ==> call.approvals == 1 && call.dispatches == 0)
    &&& (call.phase is Dispatched ==> call.dispatches == 1)
    &&& (call.shape is None ==> call.approvals == 0)
}

/// Whether envelope `id` was already delivered.
pub open spec fn seen(envelopes: Seq<EnvelopeId>, id: EnvelopeId) -> bool {
    exists|at: int| 0 <= at < envelopes.len() && #[trigger] envelopes[at] == id
}

/// C1: a grant names the user's command, or a rule of the rules in force
/// (by the seq of their record) that fits this actor's own name and call
/// `index`. Nothing else is an input: not the class, not who spawned the
/// actor.
pub open spec fn grant_fits(by: Grant, view: CoreView, index: int) -> bool {
    match by {
        Grant::User => true,
        Grant::Rule { rule, rules } => match (view.rules, view.calls[index].shape) {
            (Some((seq, in_force)), Some(shape)) => seq == rules && rule < in_force.len() && fits(
                in_force[rule as int],
                view.name,
                shape,
            ),
            _ => false,
        },
    }
}

pub open spec fn ledger_ok(calls: Seq<CallView>) -> bool {
    forall|index: int| 0 <= index < calls.len() ==> call_ok(#[trigger] calls[index])
}

/// Every call in `from..to` is resolved.
pub open spec fn resolved(calls: Seq<CallView>, from: int, to: int) -> bool {
    forall|index: int| from <= index < to ==> (#[trigger] calls[index]).phase is Resolved
}

/// The turn's part of the invariant: the ledger holds, and only the
/// current step's calls may be unresolved (none, outside `Tools`).
pub open spec fn turn_ok(view: CoreView) -> bool {
    &&& ledger_ok(view.calls)
    &&& match view.turn {
        Turn::Idle => resolved(view.calls, 0, view.calls.len() as int),
        Turn::Open(open) => {
            &&& open.step.turn == open.turn
            &&& open.first_call <= view.calls.len()
            &&& match open.stage {
                Stage::Tools => resolved(view.calls, 0, open.first_call as int),
                Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Compacting {
                    ..
                } => resolved(view.calls, 0, view.calls.len() as int),
            }
        },
    }
}

/// The actor's part of the invariant: a turn that has not called its model
/// yet has mail to carry; tasks are named by distinct seqs of earlier
/// records; and every envelope the actor ever held (carried, then still
/// waiting) appears once (T4), each a delivered one or the notification of a
/// task that ended (T4).
pub open spec fn actor_ok(view: CoreView) -> bool {
    let ids = held(view.consumed, view.mailbox);
    &&& match view.turn {
        Turn::Open(open) => awaits_mail(open.stage) ==> view.mailbox.len() > 0,
        Turn::Idle => true,
    }
    &&& forall|at: int| 0 <= at < view.tasks.len() ==> (#[trigger] view.tasks[at]).seq < view.seq
    &&& forall|first: int, second: int|
        0 <= first < second < view.tasks.len() ==> (#[trigger] view.tasks[first]).seq < (
        #[trigger] view.tasks[second]).seq
    &&& ids.no_duplicates()
    &&& forall|at: int| 0 <= at < ids.len() ==> held_ok(view, #[trigger] ids[at])
}

/// A stage whose next step is the turn's first: it carries mail.
pub open spec fn awaits_mail(stage: Stage) -> bool {
    match stage {
        Stage::Starting => true,
        Stage::Compacting { then, .. } => then is Starting,
        Stage::Model { .. } | Stage::Backoff { .. } | Stage::Tools => false,
    }
}

/// Where an envelope the actor holds came from: a delivery, or the end of
/// a task that is over (its notification).
pub open spec fn held_ok(view: CoreView, id: EnvelopeId) -> bool {
    match id {
        EnvelopeId::Task(task) => task.0 < view.seq && ended(view.tasks, task.0),
        EnvelopeId::User(_) | EnvelopeId::Minted { .. } => seen(view.envelopes, id),
    }
}

/// The invariant of every state a log reaches.
pub open spec fn state_ok(view: CoreView) -> bool {
    turn_ok(view) && actor_ok(view)
}

pub open spec fn fresh_call(shape: Option<ShapeView>) -> CallView {
    CallView {
        phase: Phase::Proposed,
        class: match shape {
            Some(shape) => Some(class_of(shape.tool)),
            None => None,
        },
        shape,
        approvals: 0,
        dispatches: 0,
    }
}

pub open spec fn initial() -> CoreView {
    CoreView {
        epoch: 0,
        seq: 0,
        name: None,
        actor: None,
        depth: 0,
        stopped: false,
        rules: None,
        envelopes: Seq::empty(),
        mailbox: Seq::empty(),
        consumed: Seq::empty(),
        turns: 0,
        turn: Turn::Idle,
        calls: Seq::empty(),
        tasks: Seq::empty(),
        children: Seq::empty(),
        compacting: None,
    }
}

/// The shape a proposed request asks for; `None` when it is invalid.
pub open spec fn request_shape<P: Payload>(request: Request<P>) -> Option<ShapeView> {
    match request {
        Request::Run { args } => Some(P::spec_shape(&args)),
        Request::Invalid { .. } => None,
    }
}

/// The calls a `StepFinished` adds.
pub open spec fn proposed<P: Payload>(calls: Seq<Proposal<P>>) -> Seq<CallView> {
    Seq::new(calls.len(), |index: int| fresh_call(request_shape(calls[index].request)))
}

/// Whether a call of `shape` starts a task.
pub open spec fn background(shape: Option<ShapeView>) -> bool {
    match shape {
        Some(shape) => shape.background,
        None => false,
    }
}

/// Whether a call of `shape` cancels a task.
pub open spec fn cancels(shape: Option<ShapeView>) -> bool {
    match shape {
        Some(shape) => shape.tool == Tool::Cancel,
        None => false,
    }
}

/// T3: a result claims no more than v knows. One the core synthesizes
/// (`reported` false) is `NotStarted` only for a call never dispatched, and
/// `Unknown` for one that was (a dispatched call may have acted), except
/// for what the core itself did at dispatch: a background call's start
/// (`Ok`: its task runs) and a `cancel` (`Ok` or `Failed`). A run's own
/// report (`reported`) comes only for a dispatched call; whether a run that
/// says it never began really did not act is the world's to tell, and the
/// replay checks it.
pub open spec fn result_fits(phase: Phase, shape: Option<ShapeView>, exit: Exit, reported: bool) -> bool {
    match phase {
        Phase::Proposed | Phase::Approved => !reported && exit is NotStarted,
        Phase::Dispatched { .. } => {
            ||| reported
            ||| exit is Unknown
            ||| (exit is Ok && background(shape))
            ||| ((exit is Ok || exit is Failed) && cancels(shape))
        },
        Phase::Resolved => false,
    }
}

/// A turn ends `Cancelled` exactly when the user asked it to stop.
pub open spec fn ending_fits(intent: Intent, outcome: TurnOutcome) -> bool {
    (intent is Cancel) == (outcome is Cancelled)
}

/// Born and not stopped: the actor may start things.
pub open spec fn live(view: CoreView) -> bool {
    view.actor is Some && !view.stopped
}

/// Whether a task started by record `seq` is running.
pub open spec fn running(tasks: Seq<Task>, seq: u64) -> bool {
    exists|at: int| 0 <= at < tasks.len() && (#[trigger] tasks[at]).seq == seq && !tasks[at].ended
}

/// Whether every task started by record `seq` has ended (true when there is
/// none).
pub open spec fn ended(tasks: Seq<Task>, seq: u64) -> bool {
    forall|at: int| 0 <= at < tasks.len() && (#[trigger] tasks[at]).seq == seq ==> tasks[at].ended
}

/// `tasks` with the task started by record `seq` ended.
pub open spec fn end_task(tasks: Seq<Task>, seq: u64) -> Seq<Task> {
    Seq::new(
        tasks.len(),
        |at: int| if tasks[at].seq == seq {
            Task { ended: true, ..tasks[at] }
        } else {
            tasks[at]
        },
    )
}

/// T3 for a task's end the core synthesizes: `Unknown` (it may have acted),
/// or `NotStarted` only for a `Safe` one (a timer has no effect to have
/// had).
pub open spec fn end_fits(tasks: Seq<Task>, seq: u64, exit: Exit, reported: bool) -> bool {
    ||| reported
    ||| exit is Unknown
    ||| (exit is NotStarted && forall|at: int|
        0 <= at < tasks.len() && (#[trigger] tasks[at]).seq == seq ==> tasks[at].class == Class::Safe)
}

/// Whether a stop task for `child` exists.
pub open spec fn stopping(tasks: Seq<Task>, child: ActorId) -> bool {
    exists|at: int| 0 <= at < tasks.len() && (#[trigger] tasks[at]).origin == Origin::Stop { child }
}

/// Whether a dispatched `spawn` named `child`.
pub open spec fn spawned(children: Seq<ActorId>, child: ActorId) -> bool {
    exists|at: int| 0 <= at < children.len() && #[trigger] children[at] == child
}

/// What an envelope's text adds to the conversation.
pub open spec fn body_bytes<P: Payload>(body: Body<P>) -> u64 {
    match body {
        Body::Text(text) => text.bytes,
        Body::Stop => 0,
    }
}

/// What a task's end adds to the conversation.
pub open spec fn output_bytes<T>(output: Option<Measured<T>>) -> u64 {
    match output {
        Some(output) => output.bytes,
        None => 0,
    }
}

/// The batch carried by a step that starts from `stage`, if `carries` is
/// the one it must carry: nothing for a retry, the batch at the head of the
/// mailbox otherwise (and something, for a turn's first step).
pub open spec fn carries_fit(stage: Stage, mailbox: Seq<Waiting>, carries: Seq<EnvelopeId>) -> bool {
    match stage {
        Stage::Backoff { .. } => carries.len() == 0,
        Stage::Starting => carries.len() > 0 && is_batch(mailbox, carries.len() as int) && carries
            == ids_of(mailbox, carries.len() as int),
        Stage::Tools => is_batch(mailbox, carries.len() as int) && carries == ids_of(
            mailbox,
            carries.len() as int,
        ),
        Stage::Model { .. } | Stage::Compacting { .. } => false,
    }
}

/// `tasks` with every task started by record `seq` marked cancelled.
pub open spec fn mark_cancelled(tasks: Seq<Task>, seq: u64) -> Seq<Task> {
    Seq::new(
        tasks.len(),
        |at: int| if tasks[at].seq == seq {
            Task { cancelled: true, ..tasks[at] }
        } else {
            tasks[at]
        },
    )
}

/// What a `Dispatched` of call `index` does besides its phase: a background
/// call starts a task, a `cancel` marks the task it names cancelled, and a
/// `spawn` names a child.
pub open spec fn dispatch(view: CoreView, index: int) -> CoreView {
    let shape = view.calls[index].shape;
    CoreView {
        tasks: match shape {
            Some(shape) => if shape.background {
                view.tasks.push(
                    Task {
                        seq: view.seq,
                        class: class_of(shape.tool),
                        origin: Origin::Call { call: CallIx(index as usize) },
                        ended: false,
                        cancelled: false,
                    },
                )
            } else {
                match shape.task {
                    Some(task) => mark_cancelled(view.tasks, task.0),
                    None => view.tasks,
                }
            },
            None => view.tasks,
        },
        children: match shape {
            Some(shape) => match shape.child {
                Some(child) => view.children.push(child),
                None => view.children,
            },
            None => view.children,
        },
        ..view
    }
}

/// What one record does to the abstract state; `None` when it cannot
/// follow it.
pub open spec fn spec_apply<P: Payload>(view: CoreView, record: Record<P>) -> Option<CoreView> {
    let seq = (view.seq + 1) as u64;
    match record {
        Record::Born { born } => if view.name is None {
            Some(CoreView { seq, name: Some(born.name@), actor: Some(born.actor), depth: born.depth, ..view })
        } else {
            None
        },
        Record::RulesLoaded { rules, .. } => Some(
            CoreView { seq, rules: Some((view.seq, rules@)), ..view },
        ),
        Record::Delivered { envelope } => if view.actor == Some(envelope.to) && !view.stopped
            && !(envelope.id is Task) && !seen(view.envelopes, envelope.id) {
            let envelopes = view.envelopes.push(envelope.id);
            match envelope.body {
                Body::Text(text) => Some(
                    CoreView {
                        seq,
                        envelopes,
                        mailbox: view.mailbox.push(
                            Waiting { id: envelope.id, user: envelope.from is User, bytes: text.bytes },
                        ),
                        ..view
                    },
                ),
                Body::Stop => Some(CoreView { seq, envelopes, stopped: true, ..view }),
            }
        } else {
            None
        },
        Record::TurnStarted { turn } => match view.turn {
            Turn::Idle => if live(view) && turn.0 == view.seq && view.mailbox.len() > 0 {
                Some(
                    CoreView {
                        seq,
                        turns: view.turns + 1,
                        turn: Turn::Open(
                            Open {
                                turn,
                                step: StepId { turn, index: 0 },
                                retries: 0,
                                stage: Stage::Starting,
                                intent: Intent::Proceed,
                                first_call: 0,
                            },
                        ),
                        calls: Seq::empty(),
                        compacting: None,
                        ..view
                    },
                )
            } else {
                None
            },
            Turn::Open(_) => None,
        },
        Record::StepStarted { step, carries } => match view.turn {
            Turn::Idle => None,
            Turn::Open(open) => {
                if step.turn != open.turn || open.intent is Cancel || view.stopped || !carries_fit(
                    open.stage,
                    view.mailbox,
                    carries@,
                ) {
                    None
                } else {
                    let retries: Option<usize> = match open.stage {
                        Stage::Starting => if step.index == 0 {
                            Some(0)
                        } else {
                            None
                        },
                        Stage::Backoff { .. } => if step.index == open.step.index {
                            Some(open.retries)
                        } else {
                            None
                        },
                        Stage::Tools => if open.step.index < u32::MAX && step.index
                            == open.step.index + 1 && resolved(
                            view.calls,
                            open.first_call as int,
                            view.calls.len() as int,
                        ) {
                            Some(0)
                        } else {
                            None
                        },
                        Stage::Model { .. } | Stage::Compacting { .. } => None,
                    };
                    match retries {
                        Some(retries) => Some(
                            CoreView {
                                seq,
                                turn: Turn::Open(
                                    Open { step, retries, stage: Stage::Model { since: view.seq }, ..open },
                                ),
                                mailbox: view.mailbox.skip(carries@.len() as int),
                                consumed: view.consumed + carries@,
                                ..view
                            },
                        ),
                        None => None,
                    }
                }
            },
        },
        Record::ModelOutput { step, .. } => match view.turn {
            Turn::Open(open) => if open.stage is Model && step == open.step {
                Some(CoreView { seq, ..view })
            } else {
                None
            },
            Turn::Idle => None,
        },
        Record::StepReset { step, reason } => match view.turn {
            Turn::Open(open) => {
                if !(open.stage is Model) || open.intent is Cancel || view.stopped || step != open.step {
                    None
                } else {
                    let retries = match reason {
                        Reset::Retry { .. } => open.retries + 1,
                        Reset::Restart => open.retries as int,
                    };
                    if retries > usize::MAX {
                        None
                    } else {
                        Some(
                            CoreView {
                                seq,
                                turn: Turn::Open(
                                    Open {
                                        retries: retries as usize,
                                        stage: Stage::Backoff { since: view.seq, reset: reason },
                                        ..open
                                    },
                                ),
                                ..view
                            },
                        )
                    }
                }
            },
            Turn::Idle => None,
        },
        Record::StepFinished { step, calls } => match view.turn {
            Turn::Open(open) => if open.stage is Model && step == open.step {
                Some(
                    CoreView {
                        seq,
                        turn: Turn::Open(
                            Open {
                                stage: Stage::Tools,
                                first_call: view.calls.len() as usize,
                                ..open
                            },
                        ),
                        calls: view.calls + proposed(calls@),
                        ..view
                    },
                )
            } else {
                None
            },
            Turn::Idle => None,
        },
        Record::Approved { call, by } => match view.turn {
            Turn::Open(open) => {
                let index = call.0 as int;
                if open.stage is Tools && open.intent is Proceed && !view.stopped && open.first_call
                    <= index < view.calls.len() && view.calls[index].phase is Proposed
                    && view.calls[index].shape is Some && grant_fits(by, view, index) {
                    Some(
                        CoreView {
                            seq,
                            calls: view.calls.update(
                                index,
                                CallView {
                                    phase: Phase::Approved,
                                    approvals: view.calls[index].approvals + 1,
                                    ..view.calls[index]
                                },
                            ),
                            ..view
                        },
                    )
                } else {
                    None
                }
            },
            Turn::Idle => None,
        },
        Record::Dispatched { call } => match view.turn {
            Turn::Open(open) => {
                let index = call.0 as int;
                if open.stage is Tools && open.intent is Proceed && !view.stopped && open.first_call
                    <= index < view.calls.len() && view.calls[index].phase is Approved {
                    Some(
                        CoreView {
                            seq,
                            calls: view.calls.update(
                                index,
                                CallView {
                                    phase: Phase::Dispatched { seq: view.seq },
                                    dispatches: view.calls[index].dispatches + 1,
                                    ..view.calls[index]
                                },
                            ),
                            ..dispatch(view, index)
                        },
                    )
                } else {
                    None
                }
            },
            Turn::Idle => None,
        },
        Record::ToolResult { call, exit, output } => match view.turn {
            Turn::Open(open) => {
                let index = call.0 as int;
                if open.stage is Tools && open.first_call <= index < view.calls.len()
                    && !(view.calls[index].phase is Resolved) && result_fits(
                    view.calls[index].phase,
                    view.calls[index].shape,
                    exit,
                    output is Some,
                ) {
                    Some(
                        CoreView {
                            seq,
                            calls: view.calls.update(
                                index,
                                CallView { phase: Phase::Resolved, ..view.calls[index] },
                            ),
                            ..view
                        },
                    )
                } else {
                    None
                }
            },
            Turn::Idle => None,
        },
        Record::TaskEnded { task, exit, output } => if running(view.tasks, task.0) && end_fits(
            view.tasks,
            task.0,
            exit,
            output is Some,
        ) {
            Some(
                CoreView {
                    seq,
                    tasks: end_task(view.tasks, task.0),
                    mailbox: view.mailbox.push(
                        Waiting { id: EnvelopeId::Task(task), user: false, bytes: output_bytes(output) },
                    ),
                    ..view
                },
            )
        } else {
            None
        },
        Record::StopSent { child } => if view.stopped && spawned(view.children, child) && !stopping(
            view.tasks,
            child,
        ) {
            Some(
                CoreView {
                    seq,
                    tasks: view.tasks.push(
                        Task {
                            seq: view.seq,
                            class: Class::Keyed,
                            origin: Origin::Stop { child },
                            ended: false,
                            cancelled: false,
                        },
                    ),
                    ..view
                },
            )
        } else {
            None
        },
        Record::CancelRequested { turn } => match view.turn {
            Turn::Open(open) => if turn == open.turn && open.intent is Proceed {
                Some(
                    CoreView { seq, turn: Turn::Open(Open { intent: Intent::Cancel, ..open }), ..view },
                )
            } else {
                None
            },
            Turn::Idle => None,
        },
        Record::TurnEnded { turn, outcome } => match view.turn {
            Turn::Open(open) => if turn == open.turn && ending_fits(open.intent, outcome)
                && resolved(view.calls, 0, view.calls.len() as int) {
                Some(CoreView { seq, turn: Turn::Idle, ..view })
            } else {
                None
            },
            Turn::Idle => None,
        },
        Record::EpochBumped { epoch } => if epoch.0 == view.epoch + 1 {
            Some(CoreView { seq, epoch: epoch.0, ..view })
        } else {
            None
        },
        Record::CompactionStarted { through } => if through == view.seq && !view.stopped {
            match view.turn {
                Turn::Idle => if view.actor is Some {
                    Some(CoreView { seq, compacting: Some(view.seq), ..view })
                } else {
                    None
                },
                Turn::Open(open) => {
                    let then = match open.stage {
                        Stage::Starting => Some(Then::Starting),
                        Stage::Tools => Some(Then::Tools),
                        Stage::Compacting { then, .. } => Some(then),
                        Stage::Model { .. } | Stage::Backoff { .. } => None,
                    };
                    match then {
                        Some(then) => if open.intent is Proceed && resolved(
                            view.calls,
                            0,
                            view.calls.len() as int,
                        ) {
                            Some(
                                CoreView {
                                    seq,
                                    turn: Turn::Open(
                                        Open { stage: Stage::Compacting { since: view.seq, then }, ..open },
                                    ),
                                    ..view
                                },
                            )
                        } else {
                            None
                        },
                        None => None,
                    }
                },
            }
        } else {
            None
        },
        Record::Compacted { started, .. } => if view.stopped {
            None
        } else {
            match view.turn {
                Turn::Idle => if view.compacting == Some(started) {
                    Some(CoreView { seq, compacting: None, ..view })
                } else {
                    None
                },
                Turn::Open(open) => match open.stage {
                    Stage::Compacting { since, then } => if since == started && open.intent is Proceed {
                        Some(
                            CoreView {
                                seq,
                                turn: Turn::Open(Open { stage: spec_stage(then), ..open }),
                                ..view
                            },
                        )
                    } else {
                        None
                    },
                    Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Tools => None,
                },
            }
        },
    }
}

/// The state `records` lead to from `view`, applied in order; `None` when
/// one of them cannot follow.
pub open spec fn applies<P: Payload>(view: CoreView, records: Seq<Record<P>>) -> Option<CoreView>
    decreases records.len(),
{
    if records.len() == 0 {
        Some(view)
    } else {
        match applies(view, records.drop_last()) {
            Some(before) => spec_apply(before, records.last()),
            None => None,
        }
    }
}

/// The state a log describes: `spec_apply` folded from `initial()`.
pub open spec fn replay<P: Payload>(log: Seq<Record<P>>) -> Option<CoreView> {
    applies(initial(), log)
}

/// An approved call may run in `view`: its `Approved` and `Dispatched`
/// records are applied, and the `Dispatched` is the record its effect id
/// names, in the current epoch.
pub open spec fn licensed<P: Payload>(view: CoreView, call: ToolCall<Approved, P>) -> bool {
    let index = call.spec_call().0 as int;
    let effect = call.spec_effect();
    &&& 0 <= index < view.calls.len()
    &&& view.calls[index].phase == Phase::Dispatched { seq: effect.seq }
    &&& view.calls[index].approvals == 1
    &&& view.calls[index].dispatches == 1
    &&& effect.epoch.0 == view.epoch
}

/// A tool run whose `Dispatched` record is one of the decision's own: its
/// effect names a record at or after the decision's first (`start.seq`).
pub open spec fn fresh<P: Payload>(start: CoreView, call: ToolCall<Approved, P>) -> bool {
    call.spec_effect().seq >= start.seq
}

/// The running turn proceeds and the actor is live: no cancel or stop
/// revoked what it holds.
pub open spec fn proceeding(view: CoreView) -> bool {
    &&& view.turn is Open
    &&& view.turn->Open_0.intent is Proceed
    &&& !view.stopped
}

/// A rerun after recovery: the call was dispatched before the decision, it
/// is `Safe` (no side effect) or `Keyed` (its target deduplicates it), it
/// runs in a new epoch, so its effect id differs from the run it repeats,
/// and its turn still proceeds: nothing reruns under a durable cancel or
/// stop (C3). An `Unsafe` call never reruns.
pub open spec fn rerun<P: Payload>(start: CoreView, end: CoreView, call: ToolCall<Approved, P>) -> bool {
    let index = call.spec_call().0 as int;
    &&& call.spec_effect().seq < start.seq
    &&& end.epoch > start.epoch
    &&& end.calls[index].class is Some
    &&& end.calls[index].class != Some(Class::Unsafe)
    &&& proceeding(end)
}

/// Task `at` of `end` may run as effect `id`: it is running and `id`
/// names its starting record in the current epoch; and that record is one
/// of the decision's own, or (only if `reruns`) the task is not `Unsafe`,
/// runs again in a new epoch, and is a stop or belongs to an actor not
/// stopped (C3).
pub open spec fn task_runs(start: CoreView, end: CoreView, at: int, id: EffectId, reruns: bool) -> bool {
    &&& 0 <= at < end.tasks.len()
    &&& end.tasks[at].seq == id.seq
    &&& !end.tasks[at].ended
    &&& id.epoch.0 == end.epoch
    &&& (id.seq >= start.seq || (reruns && end.epoch > start.epoch && end.tasks[at].class
        != Class::Unsafe && (end.tasks[at].origin is Stop || !end.stopped)))
}

/// C1 for a tool run: licensed by its call's `Dispatched`, or by a running
/// task's, started by that very call.
pub open spec fn run_ok<P: Payload>(
    start: CoreView,
    end: CoreView,
    call: ToolCall<Approved, P>,
    reruns: bool,
) -> bool {
    ||| (licensed(end, call) && (fresh(start, call) || (reruns && rerun(start, end, call))))
    ||| exists|at: int| #[trigger] task_runs(start, end, at, call.spec_effect(), reruns)
        && end.tasks[at].origin == Origin::Call { call: call.spec_call() }
}

/// C1 for a stop: licensed by a running stop task for `child`, whose
/// `StopSent` `id` names.
pub open spec fn stop_ok(start: CoreView, end: CoreView, id: EffectId, child: ActorId, reruns: bool) -> bool {
    exists|at: int| #[trigger] task_runs(start, end, at, id, reruns) && end.tasks[at].origin
        == Origin::Stop { child }
}

/// Every `RunTool` and `Stop` in `effects` is licensed by `end` and
/// started by the decision itself, or (only if `reruns`) a rerun after
/// recovery.
pub open spec fn runs_ok<P: Payload>(
    start: CoreView,
    end: CoreView,
    effects: Seq<Effect<P>>,
    reruns: bool,
) -> bool {
    forall|index: int| 0 <= index < effects.len() ==> effect_ok(start, end, #[trigger] effects[index], reruns)
}

/// The seq of the `CompactionStarted` whose summary `view` waits for: the
/// idle actor's, or the running turn's while it proceeds.
pub open spec fn live_compaction(view: CoreView) -> Option<u64> {
    match view.turn {
        Turn::Idle => view.compacting,
        Turn::Open(open) => match open.stage {
            Stage::Compacting { since, .. } => if open.intent is Proceed {
                Some(since)
            } else {
                None
            },
            Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Tools => None,
        },
    }
}

/// T1 for a summarize call: its cut, the `CompactionStarted` its id names,
/// is one of the decision's own records, durable before it starts, and the
/// live one, in the current epoch, of an actor not stopped.
pub open spec fn summarize_ok(start: CoreView, end: CoreView, id: EffectId) -> bool {
    &&& id.seq >= start.seq
    &&& id.epoch.0 == end.epoch
    &&& !end.stopped
    &&& live_compaction(end) == Some(id.seq)
}

/// `effect` is licensed, if it is a tool run, a stop or a summarize call.
pub open spec fn effect_ok<P: Payload>(start: CoreView, end: CoreView, effect: Effect<P>, reruns: bool) -> bool {
    match effect {
        Effect::RunTool { call } => run_ok(start, end, call, reruns),
        Effect::Stop { id, child } => stop_ok(start, end, id, child, reruns),
        Effect::Summarize { id } => summarize_ok(start, end, id),
        _ => true,
    }
}

/// What `decide` promises: the records apply to `start` one after another,
/// and every effect it starts is licensed by the result and started by one
/// of its own records. So an effect runs at most once per licensing record,
/// and nothing a decision did not license is ever started by it.
pub open spec fn decided<P: Payload>(start: CoreView, decision: Decision<P>) -> bool {
    match applies(start, decision.records@) {
        Some(end) => state_ok(end) && runs_ok(start, end, decision.effects@, false),
        None => false,
    }
}

/// What `resume` promises: as `decided`, except that an effect licensed
/// before the crash may run again if it is not `Unsafe`, in the new epoch.
pub open spec fn resumed<P: Payload>(start: CoreView, decision: Decision<P>) -> bool {
    match applies(start, decision.records@) {
        Some(end) => state_ok(end) && runs_ok(start, end, decision.effects@, true),
        None => false,
    }
}

/// A record the core does not decide on its own: the user's grant (C1),
/// or a run's report (T3). A decision holds one only as its first record,
/// and only from the event that brings it (`honest`).
pub open spec fn given<P: Payload>(record: Record<P>) -> bool {
    match record {
        Record::Approved { by, .. } => by is User,
        Record::ToolResult { output, .. } => output is Some,
        Record::TaskEnded { output, .. } => output is Some,
        _ => false,
    }
}

/// `event` brings `record`: the user's approval command a `Grant::User`,
/// and a run's report the result it records, exactly as reported.
pub open spec fn gives<P: Payload>(event: Event<P>, record: Record<P>) -> bool {
    match event {
        Event::Approve { verdict, .. } => record is Approved && verdict == Verdict::Approve,
        Event::ToolDone { exit, output, .. } => match record {
            Record::ToolResult { exit: recorded, output: reported, .. } => recorded == exit && reported
                == Some(output),
            Record::TaskEnded { exit: recorded, output: reported, .. } => recorded == exit && reported
                == Some(output),
            _ => false,
        },
        _ => false,
    }
}

/// What `decide` records of the world and the user is what `event` says
/// (C1, T3): a user's grant only on the user's approval command, a run's
/// report only on that run's `ToolDone`, exactly as it came, and each only
/// as the decision's first record. Every other record is the core's own.
pub open spec fn honest<P: Payload>(event: Event<P>, records: Seq<Record<P>>) -> bool {
    forall|index: int| 0 <= index < records.len() && given(#[trigger] records[index]) ==> index == 0 && gives(
        event,
        records[index],
    )
}

/// `resume` records nothing of the world or the user: no user's grant, no
/// run's report.
pub open spec fn nothing_given<P: Payload>(records: Seq<Record<P>>) -> bool {
    forall|index: int| 0 <= index < records.len() ==> !given(#[trigger] records[index])
}

/// The running turn is `turn`, and the user asked it to stop.
pub open spec fn cancelled(view: CoreView, turn: TurnId) -> bool {
    match view.turn {
        Turn::Open(open) => open.turn == turn && open.intent is Cancel,
        Turn::Idle => false,
    }
}

/// A record that starts something within a turn: a model call, a retry,
/// an approval, a dispatch, a compaction.
pub open spec fn starts<P: Payload>(record: Record<P>) -> bool {
    ||| record is StepStarted
    ||| record is StepReset
    ||| record is Approved
    ||| record is Dispatched
    ||| record is CompactionStarted
}

/// C3, one record: once a cancel of the running turn is durable, nothing
/// that starts something applies, and the turn stays cancelled until it
/// ends.
pub proof fn lemma_cancel_revokes<P: Payload>(view: CoreView, turn: TurnId, record: Record<P>)
    requires
        cancelled(view, turn),
        spec_apply(view, record) is Some,
    ensures
        !starts(record),
        !(record is TurnEnded) ==> cancelled(spec_apply(view, record)->0, turn),
{
}

/// C3, a log: after a durable cancel, no record up to the turn's end starts
/// anything, and the turn is still cancelled.
pub proof fn lemma_nothing_starts_after_a_cancel<P: Payload>(
    view: CoreView,
    turn: TurnId,
    records: Seq<Record<P>>,
)
    requires
        cancelled(view, turn),
        applies(view, records) is Some,
        forall|at: int| 0 <= at < records.len() ==> !(#[trigger] records[at] is TurnEnded),
    ensures
        forall|at: int| 0 <= at < records.len() ==> !starts(#[trigger] records[at]),
        cancelled(applies(view, records)->0, turn),
    decreases records.len(),
{
    if records.len() > 0 {
        let before = records.drop_last();
        assert forall|at: int| 0 <= at < before.len() implies !(#[trigger] before[at] is TurnEnded) by {
            assert(before[at] == records[at]);
        }
        lemma_nothing_starts_after_a_cancel(view, turn, before);
        lemma_cancel_revokes(applies(view, before)->0, turn, records.last());
        assert forall|at: int| 0 <= at < records.len() implies !starts(#[trigger] records[at]) by {
            if at < before.len() {
                assert(before[at] == records[at]);
            }
        }
    }
}

/// A record that starts anything at all for an actor: what a stop revokes.
/// A turn, everything `starts`, and the delivery of more work.
pub open spec fn acts<P: Payload>(record: Record<P>) -> bool {
    ||| starts(record)
    ||| record is TurnStarted
    ||| record is Delivered
}

/// C3 for a stopped actor, one record: once its stop is durable, no turn,
/// step, retry, approval, dispatch or delivery applies, and it stays
/// stopped.
pub proof fn lemma_stop_revokes<P: Payload>(view: CoreView, record: Record<P>)
    requires
        view.stopped,
        spec_apply(view, record) is Some,
    ensures
        !acts(record),
        spec_apply(view, record)->0.stopped,
{
}

/// C3 for a stopped actor, a log: after a durable stop, no record starts
/// anything.
pub proof fn lemma_nothing_starts_after_a_stop<P: Payload>(view: CoreView, records: Seq<Record<P>>)
    requires
        view.stopped,
        applies(view, records) is Some,
    ensures
        forall|at: int| 0 <= at < records.len() ==> !acts(#[trigger] records[at]),
        applies(view, records)->0.stopped,
    decreases records.len(),
{
    if records.len() > 0 {
        let before = records.drop_last();
        lemma_nothing_starts_after_a_stop(view, before);
        lemma_stop_revokes(applies(view, before)->0, records.last());
        assert forall|at: int| 0 <= at < records.len() implies !acts(#[trigger] records[at]) by {
            if at < before.len() {
                assert(before[at] == records[at]);
            }
        }
    }
}

/// T3: a known outcome is never overwritten. Within its turn, no record
/// gives a resolved call another result, and none changes it.
pub proof fn lemma_known_outcome_stays<P: Payload>(view: CoreView, index: int, record: Record<P>)
    requires
        0 <= index < view.calls.len(),
        view.calls[index].phase is Resolved,
        spec_apply(view, record) is Some,
        !(record is TurnStarted),
    ensures
        !(record is ToolResult && record->ToolResult_call.0 == index),
        index < spec_apply(view, record)->0.calls.len(),
        spec_apply(view, record)->0.calls[index] == view.calls[index],
{
}

/// T3 for tasks: a task ends at most once. Once every task started by
/// record `seq` (an earlier one) has ended, no `TaskEnded` for it applies,
/// and none starts again.
pub proof fn lemma_a_task_ends_once<P: Payload>(view: CoreView, seq: u64, record: Record<P>)
    requires
        seq < view.seq,
        ended(view.tasks, seq),
        spec_apply(view, record) is Some,
    ensures
        !(record is TaskEnded && record->TaskEnded_task.0 == seq),
        ended(spec_apply(view, record)->0.tasks, seq),
{
    let next = spec_apply(view, record)->0;
    assert forall|at: int| 0 <= at < next.tasks.len() && (#[trigger] next.tasks[at]).seq == seq implies next.tasks[at].ended by {
        if at < view.tasks.len() {
            assert(view.tasks[at].seq == seq);
        }
    }
}

/// T4, the core's part: an envelope reaches the model only at a step
/// boundary: a step that carries one starts a turn, or follows a step whose
/// calls have all resolved.
pub proof fn lemma_carried_at_boundaries<P: Payload>(view: CoreView, record: Record<P>)
    requires
        record is StepStarted,
        record->StepStarted_carries@.len() > 0,
        spec_apply(view, record) is Some,
    ensures
        view.turn is Open,
        view.turn->Open_0.stage is Starting || (view.turn->Open_0.stage is Tools && resolved(
            view.calls,
            view.turn->Open_0.first_call as int,
            view.calls.len() as int,
        )),
{
}

/// T4 across a compaction: neither of its records touches the mailbox,
/// what was carried or the calls, so unread mail is never inside a cut and
/// stays to be carried after it.
pub proof fn lemma_cut_keeps_mail<P: Payload>(view: CoreView, record: Record<P>)
    requires
        record is CompactionStarted || record is Compacted,
        spec_apply(view, record) is Some,
    ensures
        spec_apply(view, record)->0.mailbox == view.mailbox,
        spec_apply(view, record)->0.consumed == view.consumed,
        spec_apply(view, record)->0.calls == view.calls,
{
}

/// The cut is now, at a beat: a `CompactionStarted` applies only at its
/// own seq, with every call resolved (no tool use without its result
/// inside the cut), between turns or at a step boundary of a turn that
/// proceeds.
pub proof fn lemma_cut_is_now<P: Payload>(view: CoreView, record: Record<P>)
    requires
        state_ok(view),
        record is CompactionStarted,
        spec_apply(view, record) is Some,
    ensures
        record->CompactionStarted_through == view.seq,
        resolved(view.calls, 0, view.calls.len() as int),
        match view.turn {
            Turn::Open(open) => open.intent is Proceed && (open.stage is Starting || open.stage is Tools
                || open.stage is Compacting),
            Turn::Idle => true,
        },
{
}

/// The batch at the head of a mailbox has one size.
pub proof fn lemma_batch_unique(mailbox: Seq<Waiting>, first: int, second: int)
    requires
        is_batch(mailbox, first),
        is_batch(mailbox, second),
    ensures
        first == second,
{
    if mailbox.len() > 0 && !mailbox[0].user {
        if first < second {
            assert(!mailbox[first].user);
        } else if second < first {
            assert(!mailbox[second].user);
        }
    }
}

/// Appending an element not in a sequence without duplicates keeps it
/// without duplicates.
proof fn lemma_push_distinct<T>(items: Seq<T>, item: T)
    requires
        items.no_duplicates(),
        forall|at: int| 0 <= at < items.len() ==> items[at] != item,
    ensures
        items.push(item).no_duplicates(),
{
    let pushed = items.push(item);
    assert forall|first: int, second: int|
        0 <= first < pushed.len() && 0 <= second < pushed.len() && first != second implies pushed[first]
        != pushed[second] by {
        if first < items.len() && second < items.len() {
            assert(pushed[first] == items[first] && pushed[second] == items[second]);
        }
    }
}

/// A record that changes none of the actor's mail and tasks keeps its
/// invariant.
proof fn lemma_actor_frame(view: CoreView, next: CoreView)
    requires
        actor_ok(view),
        next.consumed == view.consumed,
        next.mailbox == view.mailbox,
        next.tasks == view.tasks,
        next.seq >= view.seq,
        forall|id: EnvelopeId| seen(view.envelopes, id) ==> seen(next.envelopes, id),
        match next.turn {
            Turn::Open(open) => awaits_mail(open.stage) ==> next.mailbox.len() > 0,
            Turn::Idle => true,
        },
    ensures
        actor_ok(next),
{
    let ids = held(view.consumed, view.mailbox);
    assert forall|at: int| 0 <= at < ids.len() implies held_ok(next, #[trigger] ids[at]) by {
        assert(held_ok(view, ids[at]));
    }
}

/// Every record that applies keeps the actor's part of the invariant: in
/// particular, no envelope is ever carried twice (T4), and a task's
/// notification lands once (T4).
pub proof fn lemma_actor_keeps<P: Payload>(view: CoreView, record: Record<P>)
    requires
        actor_ok(view),
        view.seq < u64::MAX,
        spec_apply(view, record) is Some,
    ensures
        actor_ok(spec_apply(view, record)->0),
{
    let next = spec_apply(view, record)->0;
    let ids = held(view.consumed, view.mailbox);
    match record {
        Record::Delivered { envelope } => {
            assert forall|id: EnvelopeId| seen(view.envelopes, id) implies seen(next.envelopes, id) by {
                let at = choose|at: int| 0 <= at < view.envelopes.len() && view.envelopes[at] == id;
                assert(next.envelopes[at] == id);
            }
            assert(next.envelopes[view.envelopes.len() as int] == envelope.id);
            if next.mailbox == view.mailbox {
                lemma_actor_frame(view, next);
            } else {
                let added = ids_of(next.mailbox, next.mailbox.len() as int);
                assert(held(next.consumed, next.mailbox) =~= ids.push(envelope.id));
                assert forall|at: int| 0 <= at < ids.len() implies ids[at] != envelope.id by {
                    assert(held_ok(view, ids[at]));
                }
                lemma_push_distinct(ids, envelope.id);
                let next_ids = held(next.consumed, next.mailbox);
                assert forall|at: int| 0 <= at < next_ids.len() implies held_ok(next, #[trigger] next_ids[at]) by {
                    if at < ids.len() {
                        assert(next_ids[at] == ids[at]);
                        assert(held_ok(view, ids[at]));
                    }
                }
            }
        },
        Record::StepStarted { carries, .. } => {
            let n = carries@.len() as int;
            assert(n <= view.mailbox.len());
            assert(held(next.consumed, next.mailbox) =~= ids);
            assert forall|at: int| 0 <= at < ids.len() implies held_ok(next, #[trigger] ids[at]) by {
                assert(held_ok(view, ids[at]));
            }
        },
        Record::TaskEnded { task, output, .. } => {
            let found = choose|at: int| 0 <= at < view.tasks.len() && (#[trigger] view.tasks[at]).seq == task.0 && !view.tasks[at].ended;
            let notice = EnvelopeId::Task(task);
            assert(held(next.consumed, next.mailbox) =~= ids.push(notice));
            assert forall|at: int| 0 <= at < ids.len() implies ids[at] != notice by {
                assert(held_ok(view, ids[at]));
                if ids[at] == notice {
                    assert(view.tasks[found].ended);
                }
            }
            lemma_push_distinct(ids, notice);
            let next_ids = held(next.consumed, next.mailbox);
            assert forall|at: int| 0 <= at < next_ids.len() implies held_ok(next, #[trigger] next_ids[at]) by {
                if at < ids.len() {
                    assert(next_ids[at] == ids[at]);
                    assert(held_ok(view, ids[at]));
                } else {
                    assert(next_ids[at] == notice);
                }
            }
        },
        Record::Dispatched { .. } | Record::StopSent { .. } => {
            assert forall|at: int| 0 <= at < ids.len() implies held_ok(next, #[trigger] ids[at]) by {
                assert(held_ok(view, ids[at]));
                if let EnvelopeId::Task(task) = ids[at] {
                    assert forall|t: int| 0 <= t < next.tasks.len() && (#[trigger] next.tasks[t]).seq == task.0 implies next.tasks[t].ended by {
                        if t < view.tasks.len() {
                            assert(view.tasks[t].seq == next.tasks[t].seq && view.tasks[t].ended == next.tasks[t].ended);
                        }
                    }
                }
            }
        },
        _ => {
            lemma_actor_frame(view, next);
        },
    }
}

proof fn lemma_refusal_sticks<P: Payload>(view: CoreView, log: Seq<Record<P>>, at: int)
    requires
        0 <= at <= log.len(),
        applies(view, log.take(at)) is None,
    ensures
        applies(view, log) is None,
    decreases log.len() - at,
{
    if at < log.len() {
        assert(log.take(at + 1).drop_last() == log.take(at));
        lemma_refusal_sticks(view, log, at + 1);
    } else {
        assert(log.take(at) == log);
    }
}

// ---------------------------------------------------------------------------
// Executable state.
// ---------------------------------------------------------------------------

/// Conversation size so far, against `Limits::context_bytes`. Past the bound
/// it stays `Exceeded` (no wrap, no saturation) until a reset rewinds the
/// step or a `Compacted` starts it over (`restart`); a step that would pass
/// it compacts first at a beat, and otherwise fails with `Limit::Context`
/// before it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
enum Context {
    Within(u64),
    Exceeded,
}

impl Context {
    const fn grow(self, bytes: u64, bound: u64) -> Context {
        match self {
            Context::Within(size) => match size.checked_add(bytes) {
                Some(total) => if total <= bound {
                    Context::Within(total)
                } else {
                    Context::Exceeded
                },
                None => Context::Exceeded,
            },
            Context::Exceeded => Context::Exceeded,
        }
    }

    /// Past 80% of `bound`: the end of a turn compacts the history.
    #[expect(clippy::integer_division, reason = "a fifth of the bound, rounded down: the threshold errs late by under 5 bytes")]
    const fn over(self, bound: u64) -> bool {
        match self {
            Context::Within(size) => size > bound - bound / 5,
            Context::Exceeded => true,
        }
    }

    /// A conversation that starts over from a summary of `summary` bytes
    /// (measured as the shell renders it, its header included) and `open`
    /// entries of open work.
    #[expect(clippy::as_conversions, reason = "a usize fits in a u64 on every target")]
    const fn restart(summary: u64, open: usize, bound: u64) -> Context {
        match (open as u64).checked_mul(OPEN_ENTRY_BYTES) {
            Some(entries) => Context::Within(0).grow(summary, bound).grow(entries, bound),
            None => Context::Exceeded,
        }
    }
}

struct Call<P: Payload> {
    id: CallId,
    /// `None` for a request that did not parse.
    args: Option<P::Args>,
    /// The class of the call's tool, read from the arguments the record
    /// carried; `None` for an invalid request.
    class: Option<Class>,
    /// The call's tool and subject, read from its arguments; `None` for an
    /// invalid request.
    shape: Option<Shape>,
    phase: Phase,
}

impl<P: Payload> Call<P> {
    #[expect(clippy::manual_map, reason = "Verus has no closures, so no `Option::map`")]
    fn copy(&self) -> (copy: Call<P>)
        ensures
            copy.phase == self.phase,
            copy.class == self.class,
            shape_view(copy.shape) == shape_view(self.shape),
            copy.args is Some == self.args is Some,
    {
        let args = match &self.args {
            Some(args) => Some(args.clone()),
            None => None,
        };
        let shape = match &self.shape {
            Some(shape) => Some(shape.copy()),
            None => None,
        };
        Call { id: self.id.clone(), args, class: self.class, shape, phase: self.phase }
    }

    /// Whether the call is a `spawn`.
    const fn spawns(&self) -> bool {
        match &self.shape {
            Some(shape) => shape.child().is_some(),
            None => false,
        }
    }
}

/// Which dispatched calls the decision starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[verifier::external_derive]
enum Runs {
    /// Those it dispatched itself.
    New,
    /// Those too, and every `Safe` call dispatched before it: after a
    /// crash, its rerun in the new epoch.
    All,
}

/// What the rules in force say of a proposed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[verifier::external_derive]
enum Granted {
    /// The request did not parse: there is nothing to grant.
    Invalid,
    /// A rule fits it.
    Rule(Grant),
    /// No rule fits: it awaits the user.
    Ask,
}

/// Why `apply` refused a record: the log does not describe a run of this
/// core. Recovery stops at the first one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Violation {
    TurnOpen,
    NoTurn,
    WrongTurn,
    WrongStep,
    WrongStage,
    Cancelling,
    UnknownCall,
    WrongPhase,
    Unresolved,
    Epoch,
    /// A turn the user asked to stop ends other than `Cancelled`, or one
    /// they did not ends `Cancelled`.
    WrongEnding,
    /// A counter would overflow.
    Overflow,
    /// An envelope is delivered a second time.
    DuplicateEnvelope,
    /// A grant names a rule that is not in force or does not fit the call.
    WrongGrant,
    /// The actor is born a second time.
    Reborn,
    /// The record needs the actor born, and it is not.
    Unborn,
    /// The actor is stopped: nothing starts for it.
    Stopped,
    /// An envelope for another actor, or one only this log may mint.
    Mismatch,
    /// A turn starts with nothing in the mailbox.
    NoMail,
    /// A step carries other than the batch at the head of the mailbox.
    WrongCarry,
    /// No running task has that id.
    UnknownTask,
    /// A stop of an actor this one did not spawn, or stops already, or
    /// sent while this one is not stopped.
    WrongStop,
    /// A result claims more than v knows (T3): a synthesized `NotStarted`
    /// for a dispatched call, anything but `NotStarted` for an undispatched
    /// one, or a run's report for a call never dispatched.
    WrongResult,
    /// A compaction's cut is not its own record: the cut is always now.
    WrongCut,
    /// A summary for a compaction that is not the live one.
    NotCompacting,
}

/// A record `apply` refused, and its position in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Corrupt {
    pub seq: u64,
    pub violation: Violation,
}

/// What a call-started task runs: its call's arguments, kept past its turn
/// (a `Safe` one reruns after a crash). Its call's index is the task's
/// `Origin`.
struct TaskRun<P: Payload> {
    id: CallId,
    args: P::Args,
}

/// The agent's state for one actor.
pub struct Core<P: Payload> {
    limits: Limits,
    epoch: Epoch,
    /// Records applied so far: the sequence number of the next one.
    seq: u64,
    /// Who this actor is, once it is born.
    born: Option<Born>,
    /// A stop was delivered.
    stopped: bool,
    /// Every envelope delivered, for deduplication.
    envelopes: Vec<EnvelopeId>,
    /// Delivered or notified, not yet carried, in arrival order.
    mailbox: Vec<Waiting>,
    /// Ghost: every envelope a step carried.
    consumed: Ghost<Seq<EnvelopeId>>,
    /// Turns started.
    turns: u64,
    /// The rules in force and the seq of the `RulesLoaded` that holds them.
    rules: Option<(u64, Rules)>,
    turn: Turn,
    /// The calls of the running (or last) turn, indexed by `CallIx`.
    calls: Vec<Call<P>>,
    context: Context,
    /// `context` when the current step started: a reset rewinds to it, so a
    /// discarded attempt's output does not count.
    step_base: Context,
    /// Ghost audit of the calls: approvals and dispatches applied.
    ledger: Ghost<Seq<CallView>>,
    /// Every task started, in order.
    tasks: Vec<Task>,
    /// What each task runs, beside `tasks` (`None` for a stop).
    runs: Vec<Option<TaskRun<P>>>,
    /// Every actor a dispatched `spawn` names.
    children: Vec<ActorId>,
    /// The seq of the latest compaction between turns with no summary
    /// yet, its call live or not (`CoreView::compacting`).
    compacting: Option<u64>,
    /// The last turn ended at a full context, or while it compacted (Esc,
    /// a failed summary): no turn starts until the user's next message
    /// lands or a `/compact` writes its summary; other mail waits. Read
    /// from the log, so a crash keeps it.
    parked: bool,
    /// The running turn compacted at this beat: its next step starts or
    /// the turn fails, with no second compaction. Read from the log, so a
    /// crash that tears the `Compacted` from what follows it keeps it.
    compacted: bool,
    /// The children in `children` whose `spawn` was refused: never born,
    /// so not open work.
    unborn: Vec<ActorId>,
}

impl<P: Payload> View for Core<P> {
    type V = CoreView;

    closed spec fn view(&self) -> CoreView {
        CoreView {
            epoch: self.epoch.0,
            seq: self.seq,
            name: match &self.born {
                Some(born) => Some(born.name@),
                None => None,
            },
            actor: match &self.born {
                Some(born) => Some(born.actor),
                None => None,
            },
            depth: match &self.born {
                Some(born) => born.depth,
                None => 0,
            },
            stopped: self.stopped,
            rules: match &self.rules {
                Some((seq, rules)) => Some((*seq, rules@)),
                None => None,
            },
            envelopes: self.envelopes@,
            mailbox: self.mailbox@,
            consumed: self.consumed@,
            turns: self.turns as nat,
            turn: self.turn,
            calls: self.ledger@,
            tasks: self.tasks@,
            children: self.children@,
            compacting: self.compacting,
        }
    }
}

/// `value + 1`. A log of 2^64 records is a defect: crash rather than wrap.
#[verifier::external_body]
const fn bump(value: u64) -> (next: u64)
    ensures
        next == value + 1,
{
    value.checked_add(1).expect("a log holds fewer than 2^64 records")
}

/// Whether a model call that failed with `fault` is retried.
const fn retryable(fault: ModelFault) -> bool {
    match fault {
        ModelFault::Overloaded | ModelFault::Network => true,
        ModelFault::Other | ModelFault::TooLarge | ModelFault::Cancelled => false,
    }
}

/// The delay a backoff waits.
const fn backoff(reset: Reset) -> Millis {
    match reset {
        Reset::Retry { delay, .. } => delay,
        Reset::Restart => Millis::new(0),
    }
}

/// A decision with nothing to persist or start.
const fn quiet<P: Payload>(outcome: Outcome) -> (decision: Decision<P>)
    ensures
        decision.records@.len() == 0,
        decision.effects@.len() == 0,
{
    Decision { records: Vec::new(), effects: Vec::new(), outcome }
}

/// Mirrors exec call `call` in ghost call `view`.
spec fn mirrors<P: Payload>(call: Call<P>, view: CallView) -> bool {
    &&& call.phase == view.phase
    &&& call.class == view.class
    &&& shape_view(call.shape) == view.shape
    &&& (call.args is Some) == (call.class is Some)
    &&& (call.args is Some) == (call.shape is Some)
}

/// A shape as the proofs see it.
pub open spec fn shape_view(shape: Option<Shape>) -> Option<ShapeView> {
    match shape {
        Some(shape) => Some(shape@),
        None => None,
    }
}

/// A decision being drafted on a scratch copy of the core: the records so
/// far (each already applied to the copy) and the effects.
struct Plan<P: Payload> {
    /// The state the decision applies to.
    #[cfg_attr(not(verus_keep_ghost), expect(dead_code, reason = "ghost state: read only by the proofs, erased in plain builds"))]
    start: Ghost<CoreView>,
    /// The sequence number of the decision's first record.
    from: u64,
    /// The one record the event brings that the core does not decide on
    /// its own (`given`): the user's grant or a run's report, if any.
    given: Ghost<Option<Record<P>>>,
    scratch: Core<P>,
    records: Vec<Record<P>>,
    effects: Vec<Effect<P>>,
}

impl<P: Payload> Plan<P> {
    spec fn inv(&self) -> bool {
        &&& self.scratch.wf()
        &&& applies(self.start@, self.records@) == Some(self.scratch@)
        &&& self.from == self.start@.seq
        &&& forall|index: int|
            0 <= index < self.effects@.len() ==> !((#[trigger] self.effects@[index]) is RunTool) && !(
            self.effects@[index] is Stop) && !(self.effects@[index] is Summarize)
        &&& forall|index: int|
            0 <= index < self.records@.len() && given(#[trigger] self.records@[index]) ==> index == 0
                && self.given@ == Some(self.records@[index])
    }

    /// Appends `record`, applying it to the scratch copy, and returns its
    /// sequence number.
    fn emit(&mut self, record: Record<P>) -> (seq: u64)
        requires
            old(self).inv(),
            spec_apply(old(self).scratch@, record) is Some,
            given(record) ==> old(self).records@.len() == 0 && old(self).given@ == Some(record),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).from == old(self).from,
            final(self).scratch@ == spec_apply(old(self).scratch@, record)->0,
            final(self).effects@ == old(self).effects@,
            seq == old(self).scratch@.seq,
    {
        let seq = self.scratch.seq;
        let _applied = self.scratch.apply(&record);
        proof {
            // `apply` refuses exactly what `spec_apply` cannot follow.
            assert(_applied is Ok);
        };
        let ghost before = self.records@;
        self.records.push(record);
        proof {
            assert(self.records@.drop_last() == before);
        };
        seq
    }

    /// Adds an effect that is neither a tool run, a stop nor a summarize
    /// call (those come from `finish`).
    fn effect(&mut self, effect: Effect<P>)
        requires
            old(self).inv(),
            !(effect is RunTool),
            !(effect is Stop),
            !(effect is Summarize),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).from == old(self).from,
            final(self).scratch@ == old(self).scratch@,
    {
        self.effects.push(effect);
    }

    const fn id(&self, seq: u64) -> EffectId {
        EffectId { epoch: self.scratch.epoch, seq }
    }

    /// Whether the decision ended a turn.
    fn ended(&self) -> bool {
        let mut index: usize = 0;
        while index < self.records.len()
            decreases self.records@.len() - index,
        {
            if let Record::TurnEnded { .. } = &self.records[index] {
                return true;
            }
            index += 1;
        }
        false
    }

    /// The running turn on the scratch copy.
    const fn open(&self) -> (open: Option<Open>)
        ensures
            match open {
                Some(open) => self.scratch@.turn == Turn::Open(open),
                None => self.scratch@.turn is Idle,
            },
    {
        match self.scratch.turn {
            Turn::Open(open) => Some(open),
            Turn::Idle => None,
        }
    }

    /// Resolves call `index` without running it.
    fn resolve(&mut self, index: usize, exit: Exit)
        requires
            old(self).inv(),
            old(self).scratch@.turn is Open,
            old(self).scratch@.turn->Open_0.stage is Tools,
            old(self).scratch@.turn->Open_0.first_call <= index < old(self).scratch@.calls.len(),
            result_fits(
                old(self).scratch@.calls[index as int].phase,
                old(self).scratch@.calls[index as int].shape,
                exit,
                false,
            ),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).from == old(self).from,
            final(self).scratch@.turn == old(self).scratch@.turn,
            final(self).scratch@.epoch == old(self).scratch@.epoch,
            final(self).scratch@.stopped == old(self).scratch@.stopped,
            final(self).scratch@.tasks == old(self).scratch@.tasks,
            final(self).scratch@.children == old(self).scratch@.children,
            final(self).scratch@.calls.len() == old(self).scratch@.calls.len(),
            final(self).scratch@.calls[index as int].phase is Resolved,
            forall|other: int|
                0 <= other < final(self).scratch@.calls.len() && other != index ==> (
                #[trigger] final(self).scratch@.calls[other]) == old(self).scratch@.calls[other],
    {
        self.emit(Record::ToolResult { call: CallIx(index), exit, output: None });
    }

    /// Ends running task `at` without a report: the core synthesizes its
    /// end (T3: `Unknown`, or `NotStarted` for a `Safe` task).
    fn end_task(&mut self, at: usize, exit: Exit)
        requires
            old(self).inv(),
            at < old(self).scratch@.tasks.len(),
            !old(self).scratch@.tasks[at as int].ended,
            exit is Unknown || (exit is NotStarted && old(self).scratch@.tasks[at as int].class
                == Class::Safe),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).from == old(self).from,
            final(self).scratch@.turn == old(self).scratch@.turn,
            final(self).scratch@.epoch == old(self).scratch@.epoch,
            final(self).scratch@.stopped == old(self).scratch@.stopped,
            final(self).scratch@.calls == old(self).scratch@.calls,
            final(self).scratch@.children == old(self).scratch@.children,
            final(self).scratch@.tasks.len() == old(self).scratch@.tasks.len(),
            final(self).scratch@.tasks[at as int].ended,
            forall|other: int|
                0 <= other < final(self).scratch@.tasks.len() && other != at ==> (
                #[trigger] final(self).scratch@.tasks[other]) == old(self).scratch@.tasks[other],
    {
        let ghost view = self.scratch@;
        let seq = self.scratch.task_seq(at);
        proof {
            // Task seqs are distinct: `at` is the only task `seq` names.
            assert forall|other: int| 0 <= other < view.tasks.len() && (#[trigger] view.tasks[other]).seq
                == seq implies other == at by {
                if other < at {
                    assert(view.tasks[other].seq < view.tasks[at as int].seq);
                } else if other > at {
                    assert(view.tasks[at as int].seq < view.tasks[other].seq);
                }
            }
            assert(running(view.tasks, seq));
        };
        self.emit(Record::TaskEnded { task: TaskId(seq), exit, output: None });
    }

    /// Announces each call of the current step that awaits the user: a
    /// valid call still proposed (no rule granted it), unless the turn is
    /// being cancelled.
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    fn ask(&mut self)
        requires
            old(self).inv(),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).from == old(self).from,
            final(self).scratch@ == old(self).scratch@,
    {
        let Some(open) = self.open() else {
            return;
        };
        if matches!(open.intent, Intent::Cancel) {
            return;
        }
        let count = self.scratch.calls.len();
        let mut index = open.first_call;
        while index < count
            invariant
                self.inv(),
                self.start == old(self).start,
                self.given == old(self).given,
                self.from == old(self).from,
                self.scratch@ == old(self).scratch@,
                count == self.scratch.calls@.len(),
            decreases count - index,
        {
            if matches!(self.scratch.calls[index].phase, Phase::Proposed) {
                if let Some(args) = &self.scratch.calls[index].args {
                    let request = effect::pending(CallIx(index), self.scratch.calls[index].id.clone(), args.clone());
                    self.effect(Effect::AskApproval { request });
                }
            }
            index += 1;
        }
    }

    /// Starts every effect the decision licenses (tool runs, task runs,
    /// stops and a summarize call) and hands the decision over.
    fn finish(self, outcome: Outcome, runs: Runs) -> (decision: Decision<P>)
        requires
            self.inv(),
            runs is All ==> self.scratch@.epoch > self.start@.epoch,
        ensures
            match runs {
                Runs::New => decided(self.start@, decision),
                Runs::All => resumed(self.start@, decision),
            },
            forall|index: int|
                0 <= index < decision.records@.len() && given(#[trigger] decision.records@[index]) ==> index
                    == 0 && self.given@ == Some(decision.records@[index]),
    {
        let mut plan = self;
        proof {
            assert forall|at: int| 0 <= at < plan.effects@.len() implies effect_ok(
                plan.start@,
                plan.scratch@,
                #[trigger] plan.effects@[at],
                runs is All,
            ) by {}
        };
        plan.start_calls(runs);
        plan.start_tasks(runs);
        plan.start_summary(runs);
        Decision { records: plan.records, effects: plan.effects, outcome }
    }

    /// The records are final, and every effect so far is licensed.
    spec fn started(&self, reruns: bool) -> bool {
        &&& self.scratch.wf()
        &&& applies(self.start@, self.records@) == Some(self.scratch@)
        &&& self.from == self.start@.seq
        &&& reruns ==> self.scratch@.epoch > self.start@.epoch
        &&& runs_ok(self.start@, self.scratch@, self.effects@, reruns)
    }

    /// Adds a licensed effect.
    #[cfg_attr(not(verus_keep_ghost), expect(unused_variables, reason = "ghost argument: read only by the proofs, erased in plain builds"))]
    fn start(&mut self, effect: Effect<P>, reruns: Ghost<bool>)
        requires
            old(self).started(reruns@),
            effect_ok(old(self).start@, old(self).scratch@, effect, reruns@),
        ensures
            final(self).started(reruns@),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).records@ == old(self).records@,
            final(self).scratch@ == old(self).scratch@,
            final(self).scratch.calls@ == old(self).scratch.calls@,
            final(self).scratch.tasks@ == old(self).scratch.tasks@,
            final(self).scratch.runs@ == old(self).scratch.runs@,
            final(self).from == old(self).from,
    {
        let ghost before = self.effects@;
        self.effects.push(effect);
        proof {
            assert forall|at: int| 0 <= at < self.effects@.len() implies effect_ok(
                self.start@,
                self.scratch@,
                #[trigger] self.effects@[at],
                reruns@,
            ) by {
                if at < before.len() {
                    assert(self.effects@[at] == before[at]);
                }
            }
        };
    }

    /// Runs every dispatched call of the current step: those the decision
    /// dispatched, and under `Runs::All` every one not `Unsafe` dispatched
    /// before it (a rerun after a crash).
    #[verifier::loop_isolation(false)]
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    fn start_calls(&mut self, runs: Runs)
        requires
            old(self).started(runs is All),
        ensures
            final(self).started(runs is All),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).records@ == old(self).records@,
            final(self).scratch@ == old(self).scratch@,
            final(self).scratch.tasks@ == old(self).scratch.tasks@,
            final(self).scratch.runs@ == old(self).scratch.runs@,
            final(self).from == old(self).from,
    {
        let ghost reruns = runs is All;
        let ghost start = self.start@;
        let ghost scratch = self.scratch@;
        let ghost tasks = self.scratch.tasks@;
        let ghost task_runs = self.scratch.runs@;
        let ghost from = self.from;
        let ghost records = self.records@;
        // C3: nothing reruns under a durable cancel or stop. `resume`
        // resolves every call dispatched before the crash when its turn is
        // cancelled (`resume_tools`), so this only states it.
        let proceeds = match self.scratch.turn {
            Turn::Open(open) => matches!(open.intent, Intent::Proceed),
            Turn::Idle => false,
        } && !self.scratch.stopped;
        let count = self.scratch.calls.len();
        let mut index: usize = 0;
        while index < count
            invariant
                self.started(reruns),
                self.start@ == start,
                self.scratch@ == scratch,
                self.scratch.tasks@ == tasks,
                self.scratch.runs@ == task_runs,
                self.from == from,
                self.records@ == records,
                self.given == old(self).given,
                count == self.scratch.calls@.len(),
                proceeds == proceeding(self.scratch@),
            decreases count - index,
        {
            if let Phase::Dispatched { seq } = self.scratch.calls[index].phase {
                let run = seq >= self.from || (matches!(runs, Runs::All) && proceeds && matches!(
                    self.scratch.calls[index].class,
                    Some(Class::Safe | Class::Keyed),
                ));
                if run {
                    if let Some(args) = &self.scratch.calls[index].args {
                        let call = effect::approved(
                            CallIx(index),
                            self.scratch.calls[index].id.clone(),
                            args.clone(),
                            EffectId { epoch: self.scratch.epoch, seq },
                        );
                        proof {
                            assert(mirrors(self.scratch.calls@[index as int], self.scratch.ledger@[index as int]));
                            assert(call_ok(self.scratch@.calls[index as int]));
                            assert(licensed(self.scratch@, call));
                        };
                        self.start(Effect::RunTool { call }, Ghost(reruns));
                    }
                }
            }
            index += 1;
        }
    }

    /// Starts the summarize call of the live compaction, if the decision
    /// started it: its `CompactionStarted` is one of the decision's own
    /// records (T1). One a crash cut short is started again by `resume`
    /// under a new `CompactionStarted`, never rerun.
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    #[cfg_attr(not(verus_keep_ghost), expect(unused_variables, reason = "ghost argument: read only by the proofs, erased in plain builds"))]
    fn start_summary(&mut self, runs: Runs)
        requires
            old(self).started(runs is All),
        ensures
            final(self).started(runs is All),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).records@ == old(self).records@,
            final(self).scratch@ == old(self).scratch@,
            final(self).from == old(self).from,
    {
        if self.scratch.stopped {
            return;
        }
        let live = match self.scratch.turn {
            Turn::Idle => self.scratch.compacting,
            Turn::Open(open) => match (open.stage, open.intent) {
                (Stage::Compacting { since, .. }, Intent::Proceed) => Some(since),
                (Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Tools, _)
                | (Stage::Compacting { .. }, Intent::Cancel) => None,
            },
        };
        let ghost reruns = runs is All;
        if let Some(seq) = live {
            if seq >= self.from {
                let id = EffectId { epoch: self.scratch.epoch, seq };
                self.start(Effect::Summarize { id }, Ghost(reruns));
            }
        }
    }

    /// Runs every running task: a call's (a background run, a timer) or a
    /// stop. Each runs when the decision started it, and under `Runs::All`
    /// again if it is not `Unsafe` (a rerun after a crash: the same seq, so
    /// a `Keyed` stop delivers the same envelope id).
    #[verifier::loop_isolation(false)]
    fn start_tasks(&mut self, runs: Runs)
        requires
            old(self).started(runs is All),
        ensures
            final(self).started(runs is All),
            final(self).start == old(self).start,
            final(self).given == old(self).given,
            final(self).records@ == old(self).records@,
            final(self).scratch@ == old(self).scratch@,
            final(self).from == old(self).from,
    {
        let ghost reruns = runs is All;
        let ghost start = self.start@;
        let ghost scratch = self.scratch@;
        let ghost from = self.from;
        let ghost records = self.records@;
        let count = self.scratch.tasks.len();
        let mut at: usize = 0;
        while at < count
            invariant
                self.started(reruns),
                self.start@ == start,
                self.scratch@ == scratch,
                self.from == from,
                self.records@ == records,
                self.given == old(self).given,
                count == self.scratch.tasks@.len(),
                self.scratch.runs@.len() == count,
            decreases count - at,
        {
            let task = self.scratch.tasks[at];
            // C3: a stopped actor's own tasks never rerun; its stops do.
            let run = !task.ended && (task.seq >= self.from || (matches!(runs, Runs::All)
                && !matches!(task.class, Class::Unsafe) && (matches!(task.origin, Origin::Stop { .. })
                || !self.scratch.stopped)));
            if run {
                let id = EffectId { epoch: self.scratch.epoch, seq: task.seq };
                proof {
                    assert(task_runs(self.start@, self.scratch@, at as int, id, reruns));
                };
                match task.origin {
                    Origin::Call { call } => {
                        if let Some(run) = &self.scratch.runs[at] {
                            let call = effect::approved(call, run.id.clone(), run.args.clone(), id);
                            self.start(Effect::RunTool { call }, Ghost(reruns));
                        }
                    },
                    Origin::Stop { child } => {
                        self.start(Effect::Stop { id, child }, Ghost(reruns));
                    },
                }
            }
            at += 1;
        }
    }
}

/// Whether `children` names `child`.
const fn contains(children: &[ActorId], child: ActorId) -> bool {
    let mut at: usize = 0;
    while at < children.len()
        decreases children@.len() - at,
    {
        if children[at].same(child) {
            return true;
        }
        at += 1;
    }
    false
}

/// A copy of `source`.
fn copy_of<T: Copy>(source: &[T]) -> (copy: Vec<T>)
    ensures
        copy@ == source@,
{
    let mut copy: Vec<T> = Vec::with_capacity(source.len());
    let mut index: usize = 0;
    while index < source.len()
        invariant
            index <= source@.len(),
            copy@ == source@.take(index as int),
        decreases source@.len() - index,
    {
        copy.push(source[index]);
        index += 1;
        assert(copy@ =~= source@.take(index as int));
    }
    assert(source@.take(source@.len() as int) =~= source@);
    copy
}

/// `tasks` with every task started by record `seq` marked cancelled.
fn marked(tasks: &[Task], seq: u64) -> (marked: Vec<Task>)
    ensures
        marked@ == mark_cancelled(tasks@, seq),
{
    let mut marked: Vec<Task> = Vec::with_capacity(tasks.len());
    let mut at: usize = 0;
    while at < tasks.len()
        invariant
            at <= tasks@.len(),
            marked@ == mark_cancelled(tasks@, seq).take(at as int),
        decreases tasks@.len() - at,
    {
        let task = tasks[at];
        marked.push(if task.seq == seq { Task { cancelled: true, ..task } } else { task });
        at += 1;
        assert(marked@ =~= mark_cancelled(tasks@, seq).take(at as int));
    }
    assert(mark_cancelled(tasks@, seq).take(tasks@.len() as int) =~= mark_cancelled(tasks@, seq));
    marked
}

/// `source` without its first `count`.
fn skip_of<T: Copy>(source: &[T], count: usize) -> (rest: Vec<T>)
    requires
        count <= source@.len(),
    ensures
        rest@ == source@.skip(count as int),
{
    let mut rest: Vec<T> = Vec::with_capacity(source.len() - count);
    let mut index: usize = count;
    while index < source.len()
        invariant
            count <= index <= source@.len(),
            rest@ == source@.subrange(count as int, index as int),
        decreases source@.len() - index,
    {
        rest.push(source[index]);
        index += 1;
        assert(rest@ =~= source@.subrange(count as int, index as int));
    }
    assert(source@.skip(count as int) =~= source@.subrange(count as int, source@.len() as int));
    rest
}

impl<P: Payload> Core<P> {
    /// The state ready to run under `limits`.
    pub closed spec fn wf(&self) -> bool {
        &&& self.ledger@.len() == self.calls@.len()
        &&& forall|index: int|
            0 <= index < self.calls@.len() ==> mirrors(#[trigger] self.calls@[index], self.ledger@[index])
        &&& self.runs@.len() == self.tasks@.len()
        &&& state_ok(self@)
    }

    /// An unborn actor.
    #[must_use]
    pub fn new(limits: Limits) -> (core: Core<P>)
        ensures
            core.wf(),
            core@ == initial(),
    {
        let envelopes = Vec::with_capacity(limits.messages_per_actor());
        let calls = Vec::with_capacity(limits.calls_per_turn());
        let core = Core {
            limits,
            epoch: Epoch(0),
            seq: 0,
            born: None,
            stopped: false,
            envelopes,
            mailbox: Vec::new(),
            consumed: Ghost(Seq::empty()),
            turns: 0,
            rules: None,
            turn: Turn::Idle,
            calls,
            context: Context::Within(0),
            step_base: Context::Within(0),
            ledger: Ghost(Seq::empty()),
            tasks: Vec::new(),
            runs: Vec::new(),
            children: Vec::new(),
            compacting: None,
            parked: false,
            compacted: false,
            unborn: Vec::new(),
        };
        assert(core@.calls =~= initial().calls);
        assert(core@.envelopes =~= initial().envelopes);
        assert(core@.mailbox =~= initial().mailbox);
        assert(core@.tasks =~= initial().tasks);
        assert(core@.children =~= initial().children);
        assert(held(core@.consumed, core@.mailbox) =~= Seq::<EnvelopeId>::empty());
        core
    }

    /// The bounds this core runs under.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The current epoch.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The sequence number the next record gets: the number applied so far.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Who this actor is, once it is born.
    #[must_use]
    pub const fn born(&self) -> Option<&Born> {
        match &self.born {
            Some(born) => Some(born),
            None => None,
        }
    }

    /// Whether a stop was delivered.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stopped
    }

    /// The rules in force and the seq of the `RulesLoaded` that holds them,
    /// if any were ever loaded.
    #[must_use]
    pub const fn rules(&self) -> Option<&(u64, Rules)> {
        match &self.rules {
            Some(rules) => Some(rules),
            None => None,
        }
    }

    /// The running turn, if any.
    #[must_use]
    pub const fn running(&self) -> Option<TurnId> {
        match self.turn {
            Turn::Open(open) => Some(open.turn),
            Turn::Idle => None,
        }
    }

    /// Envelopes delivered and not yet carried by a model call.
    #[must_use]
    pub const fn unread(&self) -> usize {
        self.mailbox.len()
    }

    /// The turns started so far, which `Limits::turns_per_actor` bounds.
    #[must_use]
    pub const fn turns(&self) -> u64 {
        self.turns
    }

    /// A scratch copy to plan on: same state, same view.
    #[expect(clippy::manual_map, reason = "Verus has no closures, so no `Option::map`")]
    fn scratch(&self) -> (copy: Core<P>)
        requires
            self.wf(),
        ensures
            copy.wf(),
            copy@ == self@,
    {
        let mut calls: Vec<Call<P>> = Vec::with_capacity(self.calls.len());
        let mut index: usize = 0;
        while index < self.calls.len()
            invariant
                index <= self.calls@.len(),
                calls@.len() == index,
                forall|at: int|
                    0 <= at < index ==> (#[trigger] calls@[at]).phase == self.calls@[at].phase
                        && calls@[at].class == self.calls@[at].class && shape_view(calls@[at].shape)
                        == shape_view(self.calls@[at].shape) && (calls@[at].args is Some) == (
                    self.calls@[at].args is Some),
            decreases self.calls@.len() - index,
        {
            calls.push(self.calls[index].copy());
            index += 1;
        }
        let mut runs: Vec<Option<TaskRun<P>>> = Vec::with_capacity(self.runs.len());
        let mut at: usize = 0;
        while at < self.runs.len()
            invariant
                at <= self.runs@.len(),
                runs@.len() == at,
            decreases self.runs@.len() - at,
        {
            let run = match &self.runs[at] {
                Some(run) => Some(TaskRun { id: run.id.clone(), args: run.args.clone() }),
                None => None,
            };
            runs.push(run);
            at += 1;
        }
        let born = match &self.born {
            Some(born) => Some(born.copy()),
            None => None,
        };
        let rules = match &self.rules {
            Some((seq, rules)) => Some((*seq, rules.copy())),
            None => None,
        };
        let copy = Core {
            limits: self.limits.clone(),
            epoch: self.epoch,
            seq: self.seq,
            born,
            stopped: self.stopped,
            envelopes: copy_of(self.envelopes.as_slice()),
            mailbox: copy_of(self.mailbox.as_slice()),
            consumed: Ghost(self.consumed@),
            turns: self.turns,
            rules,
            turn: self.turn,
            calls,
            context: self.context,
            step_base: self.step_base,
            ledger: Ghost(self.ledger@),
            tasks: copy_of(self.tasks.as_slice()),
            runs,
            children: copy_of(self.children.as_slice()),
            compacting: self.compacting,
            parked: self.parked,
            compacted: self.compacted,
            unborn: copy_of(self.unborn.as_slice()),
        };
        proof {
            assert forall|at: int| 0 <= at < copy.calls@.len() implies mirrors(
                #[trigger] copy.calls@[at],
                copy.ledger@[at],
            ) by {
                assert(mirrors(self.calls@[at], self.ledger@[at]));
            }
        };
        copy
    }

    const fn refuse(&self, violation: Violation) -> (refused: Result<(), Corrupt>)
        ensures
            refused is Err,
    {
        Err(Corrupt { seq: self.seq, violation })
    }

    /// The running turn, for a record that needs one.
    const fn open(&self) -> (open: Result<Open, Corrupt>)
        ensures
            match open {
                Ok(open) => self@.turn == Turn::Open(open),
                Err(_) => self@.turn is Idle,
            },
    {
        match self.turn {
            Turn::Open(open) => Ok(open),
            Turn::Idle => Err(Corrupt { seq: self.seq, violation: Violation::NoTurn }),
        }
    }

    /// The index of `call`, for a record that names a call of the current
    /// step.
    const fn current_call(&self, open: Open, call: CallIx) -> (index: Result<usize, Corrupt>)
        requires
            self.wf(),
        ensures
            match index {
                Ok(index) => {
                    &&& index == call.0
                    &&& open.first_call <= index < self@.calls.len()
                    &&& mirrors(self.calls@[index as int], self.ledger@[index as int])
                },
                Err(_) => !(open.first_call <= call.0 < self@.calls.len()),
            },
    {
        let index = call.0;
        if index < open.first_call || index >= self.calls.len() {
            return Err(Corrupt { seq: self.seq, violation: Violation::UnknownCall });
        }
        assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
        Ok(index)
    }

    /// Whether every call from `from` on is resolved.
    fn all_resolved(&self, from: usize) -> (all: bool)
        requires
            self.wf(),
            from <= self@.calls.len(),
        ensures
            all == resolved(self@.calls, from as int, self@.calls.len() as int),
    {
        let mut index = from;
        let mut all = true;
        while index < self.calls.len()
            invariant
                self.wf(),
                from <= index <= self@.calls.len(),
                all == resolved(self@.calls, from as int, index as int),
            decreases self.calls@.len() - index,
        {
            assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
            if !matches!(self.calls[index].phase, Phase::Resolved) {
                all = false;
            }
            index += 1;
        }
        all
    }

    /// Whether grant `by` fits call `index` in this state: the user's
    /// command, or a rule of the rules in force that fits it.
    fn grant_fits(&self, by: Grant, index: usize) -> (fit: bool)
        requires
            self.wf(),
            index < self@.calls.len(),
        ensures
            fit == grant_fits(by, self@, index as int),
    {
        assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
        let Some(shape) = &self.calls[index].shape else {
            return matches!(by, Grant::User);
        };
        match by {
            Grant::User => true,
            Grant::Rule { rule, rules } => match &self.rules {
                Some((seq, in_force)) => {
                    #[expect(clippy::as_conversions, reason = "a usize fits in a u64 on every target")]
                    let count = in_force.rules.len() as u64;
                    if *seq != rules || rule >= count {
                        return false;
                    }
                    proof {
                        assert(in_force@[rule as int] == in_force.rules@[rule as int]@);
                    };
                    #[expect(
                        clippy::as_conversions,
                        clippy::cast_possible_truncation,
                        reason = "Verus proves the index is below the rules' length, a usize"
                    )]
                    let at = rule as usize;
                    rules::rule_fits(&in_force.rules[at], self.name(), shape)
                },
                None => false,
            },
        }
    }

    /// This actor's name, once it is born.
    const fn name(&self) -> (name: Option<&ActorName>)
        ensures
            rules::name_view(name) == self@.name,
    {
        match &self.born {
            Some(born) => Some(&born.name),
            None => None,
        }
    }

    /// What the rules in force say of call `index`: the first rule that
    /// fits it, if any (C1).
    fn rule_for(&self, index: usize) -> (granted: Granted)
        requires
            self.wf(),
            index < self@.calls.len(),
        ensures
            match granted {
                Granted::Invalid => self@.calls[index as int].shape is None,
                Granted::Rule(by) => by is Rule && self@.calls[index as int].shape is Some && grant_fits(
                    by,
                    self@,
                    index as int,
                ),
                Granted::Ask => true,
            },
    {
        assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
        let Some(shape) = &self.calls[index].shape else {
            return Granted::Invalid;
        };
        let Some((seq, rules)) = &self.rules else {
            return Granted::Ask;
        };
        match rules::grant(rules, self.name(), shape) {
            Some(rule) => Granted::Rule(Grant::Rule { rule, rules: *seq }),
            None => Granted::Ask,
        }
    }

    /// The seq of task `at`.
    fn task_seq(&self, at: usize) -> (seq: u64)
        requires
            at < self@.tasks.len(),
        ensures
            seq == self@.tasks[at as int].seq,
    {
        self.tasks[at].seq
    }

    /// Whether envelope `id` was already delivered.
    fn seen(&self, id: EnvelopeId) -> (found: bool)
        ensures
            found == seen(self@.envelopes, id),
    {
        let mut index: usize = 0;
        while index < self.envelopes.len()
            invariant
                index <= self.envelopes@.len(),
                forall|at: int| 0 <= at < index ==> #[trigger] self.envelopes@[at] != id,
            decreases self.envelopes@.len() - index,
        {
            if self.envelopes[index].same(id) {
                assert(self.envelopes@[index as int] == id);
                return true;
            }
            index += 1;
        }
        false
    }

    /// The index of the running task that record `seq` started, if any.
    fn running_task(&self, seq: u64) -> (found: Option<usize>)
        requires
            self.wf(),
        ensures
            match found {
                Some(at) => at < self@.tasks.len() && self@.tasks[at as int].seq == seq
                    && !self@.tasks[at as int].ended,
                None => !running(self@.tasks, seq),
            },
    {
        let mut at: usize = 0;
        while at < self.tasks.len()
            invariant
                at <= self@.tasks.len(),
                forall|other: int| 0 <= other < at ==> !((#[trigger] self@.tasks[other]).seq == seq
                    && !self@.tasks[other].ended),
            decreases self@.tasks.len() - at,
        {
            if self.tasks[at].seq == seq && !self.tasks[at].ended {
                return Some(at);
            }
            at += 1;
        }
        None
    }

    /// Whether the running task record `seq` started was signalled: a
    /// cancel of it was dispatched.
    fn signalled(&self, seq: u64) -> (signalled: bool)
        requires
            self.wf(),
    {
        match self.running_task(seq) {
            Some(at) => self.tasks[at].cancelled,
            None => false,
        }
    }

    /// Whether a stop task for `child` exists.
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    fn stopping(&self, child: ActorId) -> (found: bool)
        ensures
            found == stopping(self@.tasks, child),
    {
        let mut at: usize = 0;
        while at < self.tasks.len()
            invariant
                at <= self@.tasks.len(),
                forall|other: int| 0 <= other < at ==> (#[trigger] self@.tasks[other]).origin
                    != (Origin::Stop { child }),
            decreases self@.tasks.len() - at,
        {
            if let Origin::Stop { child: stopped } = self.tasks[at].origin {
                if stopped.same(child) {
                    return true;
                }
            }
            at += 1;
        }
        false
    }

    /// Whether a dispatched `spawn` named `child`.
    fn spawned(&self, child: ActorId) -> (found: bool)
        ensures
            found == spawned(self@.children, child),
    {
        let mut at: usize = 0;
        while at < self.children.len()
            invariant
                at <= self@.children.len(),
                forall|other: int| 0 <= other < at ==> #[trigger] self@.children[other] != child,
            decreases self@.children.len() - at,
        {
            if self.children[at].same(child) {
                return true;
            }
            at += 1;
        }
        false
    }

    /// The actor's address, if it is born and not stopped.
    const fn live(&self) -> (actor: Option<ActorId>)
        ensures
            match actor {
                Some(actor) => live(self@) && self@.actor == Some(actor),
                None => !live(self@),
            },
    {
        if self.stopped {
            return None;
        }
        match &self.born {
            Some(born) => Some(born.actor),
            None => None,
        }
    }

    /// What the actor has open, for a `Compacted`: its running tasks and
    /// the children it spawned (not a refused spawn's), from its own
    /// state, never the model's.
    fn open_work(&self) -> OpenWork {
        let mut tasks: Vec<TaskId> = Vec::new();
        let mut at: usize = 0;
        while at < self.tasks.len()
            decreases self.tasks@.len() - at,
        {
            let task = self.tasks[at];
            if !task.ended && matches!(task.origin, Origin::Call { .. }) {
                tasks.push(TaskId(task.seq));
            }
            at += 1;
        }
        let mut children: Vec<ActorId> = Vec::new();
        let mut at: usize = 0;
        while at < self.children.len()
            decreases self.children@.len() - at,
        {
            let child = self.children[at];
            if !contains(self.unborn.as_slice(), child) {
                children.push(child);
            }
            at += 1;
        }
        OpenWork { tasks, children }
    }

    /// `apply` of `StepFinished { step, calls }`.
    #[expect(clippy::manual_map, reason = "Verus has no closures, so no `Option::map`")]
    #[expect(clippy::ptr_arg, reason = "the specification names the record's own `Vec`")]
    fn apply_step_finished(&mut self, step: StepId, calls: &Vec<Proposal<P>>) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::StepFinished { step, calls: *calls }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let open = self.open()?;
        if !(matches!(open.stage, Stage::Model { .. })) {
            return self.refuse(Violation::WrongStage);
        }
        if !step.same(open.step) {
            return self.refuse(Violation::WrongStep);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::StepFinished { step, calls: *calls });
        };
        let first_call = self.calls.len();
        let ghost before = self.ledger@;
        let mut index: usize = 0;
        while index < calls.len()
            invariant
                index <= calls@.len(),
                self.calls@.len() == first_call + index,
                self.ledger@ == before + proposed(calls@.take(index as int)),
                before.len() == first_call,
                forall|at: int|
                    0 <= at < self.calls@.len() ==> mirrors(#[trigger] self.calls@[at], self.ledger@[at]),
            decreases calls@.len() - index,
        {
            let proposal = &calls[index];
            let (args, shape) = match &proposal.request {
                Request::Run { args } => (Some(args.clone()), Some(P::shape(args))),
                Request::Invalid { .. } => (None, None),
            };
            let class = match &shape {
                Some(shape) => Some(shape.tool().class()),
                None => None,
            };
            let ghost view = shape_view(shape);
            proof {
                assert(view == request_shape(calls@[index as int].request));
            };
            self.calls.push(Call { id: proposal.id.clone(), args, class, shape, phase: Phase::Proposed });
            self.ledger = Ghost(self.ledger@.push(fresh_call(view)));
            assert(calls@.take(index + 1).drop_last() == calls@.take(index as int));
            assert(proposed(calls@.take(index + 1)) =~= proposed(calls@.take(index as int)).push(fresh_call(view)));
            index += 1;
        }
        assert(calls@.take(calls@.len() as int) == calls@);
        self.turn = Turn::Open(Open { stage: Stage::Tools, first_call, ..open });
        self.seq = next;
        proof {
            let view = self@;
            assert(view.calls =~= before + proposed(calls@));
            assert forall|at: int| 0 <= at < view.calls.len() implies call_ok(#[trigger] view.calls[at]) by {
                if at < before.len() {
                    assert(view.calls[at] == before[at]);
                } else {
                    assert(view.calls[at] == proposed(calls@)[at - before.len()]);
                }
            }
            assert forall|at: int| 0 <= at < first_call implies (#[trigger] view.calls[at]).phase is Resolved by {
                assert(view.calls[at] == before[at]);
            }
        };
        Ok(())
    }

    /// `apply` of `Approved { call, by }`.
    fn apply_approved(&mut self, call: CallIx, by: Grant) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::Approved { call, by }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let open = self.open()?;
        if !(matches!(open.stage, Stage::Tools)) {
            return self.refuse(Violation::WrongStage);
        }
        if matches!(open.intent, Intent::Cancel) {
            return self.refuse(Violation::Cancelling);
        }
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        let index = self.current_call(open, call)?;
        if !(matches!(self.calls[index].phase, Phase::Proposed)) {
            return self.refuse(Violation::WrongPhase);
        }
        if self.calls[index].shape.is_none() {
            return self.refuse(Violation::WrongPhase);
        }
        if !self.grant_fits(by, index) {
            return self.refuse(Violation::WrongGrant);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::Approved { call, by });
        };
        let ghost was = self.ledger@[index as int];
        self.calls[index].phase = Phase::Approved;
        self.ledger = Ghost(
            self.ledger@.update(
                index as int,
                CallView { phase: Phase::Approved, approvals: was.approvals + 1, ..was },
            ),
        );
        self.seq = next;
        assert(call_ok(self.ledger@[index as int]));
        Ok(())
    }

    /// `apply` of `Delivered { envelope }`.
    fn apply_delivered(&mut self, envelope: &Envelope<P>) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::Delivered { envelope: *envelope }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let Some(born) = &self.born else {
            return self.refuse(Violation::Unborn);
        };
        if !born.actor.same(envelope.to) || matches!(envelope.id, EnvelopeId::Task(_)) {
            return self.refuse(Violation::Mismatch);
        }
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        if self.seen(envelope.id) {
            return self.refuse(Violation::DuplicateEnvelope);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::Delivered { envelope: *envelope });
        };
        self.envelopes.push(envelope.id);
        match &envelope.body {
            Body::Text(text) => {
                self.mailbox.push(
                    Waiting { id: envelope.id, user: matches!(envelope.from, Address::User), bytes: text.bytes },
                );
                if matches!(envelope.from, Address::User) {
                    self.parked = false;
                }
            },
            Body::Stop => {
                self.stopped = true;
            },
        }
        self.seq = next;
        Ok(())
    }

    /// `apply` of `TurnStarted { turn }`.
    fn apply_turn_started(&mut self, turn: TurnId) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::TurnStarted { turn }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        if matches!(self.turn, Turn::Open(_)) {
            return self.refuse(Violation::TurnOpen);
        }
        if self.live().is_none() {
            return self.refuse(if self.stopped { Violation::Stopped } else { Violation::Unborn });
        }
        if turn.0 != self.seq {
            return self.refuse(Violation::WrongTurn);
        }
        if self.mailbox.len() == 0 {
            return self.refuse(Violation::NoMail);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::TurnStarted { turn });
        };
        self.turns = bump(self.turns);
        self.turn = Turn::Open(Open {
            turn,
            step: StepId { turn, index: 0 },
            retries: 0,
            stage: Stage::Starting,
            intent: Intent::Proceed,
            first_call: 0,
        });
        self.calls.clear();
        self.ledger = Ghost(Seq::empty());
        self.compacting = None;
        self.seq = next;
        assert(self@.calls =~= spec_apply(old(self)@, Record::<P>::TurnStarted { turn })->0.calls);
        Ok(())
    }

    /// `apply` of `StepStarted { step, carries }`.
    fn apply_step_started(&mut self, step: StepId, carries: &Vec<EnvelopeId>) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::StepStarted { step, carries: *carries }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let open = self.open()?;
        if !step.turn.same(open.turn) {
            return self.refuse(Violation::WrongTurn);
        }
        if matches!(open.intent, Intent::Cancel) {
            return self.refuse(Violation::Cancelling);
        }
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        let count = carries.len();
        let batch = mailbox::batch(self.mailbox.as_slice());
        let fits = match open.stage {
            Stage::Backoff { .. } => count == 0,
            Stage::Starting => count > 0 && batch == count && mailbox::heads(carries.as_slice(), self.mailbox.as_slice()),
            Stage::Tools => batch == count && mailbox::heads(carries.as_slice(), self.mailbox.as_slice()),
            Stage::Model { .. } | Stage::Compacting { .. } => false,
        };
        proof {
            if is_batch(self@.mailbox, count as int) {
                lemma_batch_unique(self@.mailbox, batch as int, count as int);
            }
        };
        if !fits {
            return self.refuse(Violation::WrongCarry);
        }
        let retries = match open.stage {
            Stage::Starting => {
                if step.index != 0 {
                    return self.refuse(Violation::WrongStep);
                }
                0
            },
            Stage::Backoff { .. } => {
                if step.index != open.step.index {
                    return self.refuse(Violation::WrongStep);
                }
                open.retries
            },
            Stage::Tools => {
                if open.step.index == u32::MAX || step.index != open.step.index + 1 {
                    return self.refuse(Violation::WrongStep);
                }
                if !self.all_resolved(open.first_call) {
                    return self.refuse(Violation::Unresolved);
                }
                0
            },
            Stage::Model { .. } | Stage::Compacting { .. } => return self.refuse(Violation::WrongStage),
        };
        proof {
            lemma_actor_keeps(self@, Record::<P>::StepStarted { step, carries: *carries });
        };
        let (_, bytes) = mailbox::head(self.mailbox.as_slice(), count);
        self.mailbox = skip_of(self.mailbox.as_slice(), count);
        self.consumed = Ghost(self.consumed@ + carries@);
        self.turn = Turn::Open(
            Open { step, retries, stage: Stage::Model { since: self.seq }, ..open },
        );
        self.context = self.context.grow(bytes, self.limits.context_bytes());
        self.step_base = self.context;
        self.compacted = false;
        self.seq = next;
        Ok(())
    }

    /// `apply` of `Dispatched { call }`.
    fn apply_dispatched(&mut self, call: CallIx) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::Dispatched { call }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let open = self.open()?;
        if !(matches!(open.stage, Stage::Tools)) {
            return self.refuse(Violation::WrongStage);
        }
        if matches!(open.intent, Intent::Cancel) {
            return self.refuse(Violation::Cancelling);
        }
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        let index = self.current_call(open, call)?;
        if !(matches!(self.calls[index].phase, Phase::Approved)) {
            return self.refuse(Violation::WrongPhase);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::Dispatched { call });
            assert(call_ok(self@.calls[index as int]));
        };
        let ghost was = self.ledger@[index as int];
        if let Some(shape) = &self.calls[index].shape {
            if shape.background() {
                if let Some(args) = &self.calls[index].args {
                    self.tasks.push(
                        Task {
                            seq: self.seq,
                            class: shape.tool().class(),
                            origin: Origin::Call { call },
                            ended: false,
                            cancelled: false,
                        },
                    );
                    self.runs.push(Some(TaskRun { id: self.calls[index].id.clone(), args: args.clone() }));
                }
            } else if let Some(task) = shape.task() {
                self.tasks = marked(self.tasks.as_slice(), task.0);
            }
            if let Some(child) = shape.child() {
                self.children.push(child);
            }
        }
        self.calls[index].phase = Phase::Dispatched { seq: self.seq };
        self.ledger = Ghost(
            self.ledger@.update(
                index as int,
                CallView { phase: Phase::Dispatched { seq: self.seq }, dispatches: was.dispatches + 1, ..was },
            ),
        );
        self.seq = next;
        assert(call_ok(self.ledger@[index as int]));
        Ok(())
    }

    /// `apply` of `ToolResult { call, exit, output }`.
    #[expect(clippy::ref_option, reason = "the specification names the record's own field")]
    fn apply_tool_result(&mut self, call: CallIx, exit: Exit, output: &Option<Measured<P::Output>>) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::ToolResult { call, exit, output: *output }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let open = self.open()?;
        if !(matches!(open.stage, Stage::Tools)) {
            return self.refuse(Violation::WrongStage);
        }
        let index = self.current_call(open, call)?;
        let (background, cancels, child) = match &self.calls[index].shape {
            Some(shape) => (shape.background(), matches!(shape.tool(), Tool::Cancel), shape.child()),
            None => (false, false, None),
        };
        let fits = match self.calls[index].phase {
            Phase::Proposed | Phase::Approved => output.is_none() && matches!(exit, Exit::NotStarted(_)),
            Phase::Dispatched { .. } => output.is_some() || matches!(exit, Exit::Unknown) || (matches!(
                exit,
                Exit::Ok,
            ) && background) || (matches!(exit, Exit::Ok | Exit::Failed) && cancels),
            Phase::Resolved => return self.refuse(Violation::WrongPhase),
        };
        if !fits {
            return self.refuse(Violation::WrongResult);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::ToolResult { call, exit, output: *output });
        };
        if let (Phase::Dispatched { .. }, Exit::NotStarted(_), Some(child)) = (self.calls[index].phase, exit, child) {
            self.unborn.push(child);
        }
        let ghost was = self.ledger@[index as int];
        self.calls[index].phase = Phase::Resolved;
        self.ledger = Ghost(self.ledger@.update(index as int, CallView { phase: Phase::Resolved, ..was }));
        self.context = match output {
            Some(output) => self.context.grow(output.bytes, self.limits.context_bytes()),
            None => self.context,
        };
        self.seq = next;
        assert(call_ok(self.ledger@[index as int]));
        Ok(())
    }

    /// `apply` of `TaskEnded { task, exit, output }`.
    #[expect(clippy::ref_option, reason = "the specification names the record's own field")]
    fn apply_task_ended(&mut self, task: TaskId, exit: Exit, output: &Option<Measured<P::Output>>) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::TaskEnded { task, exit, output: *output }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let Some(at) = self.running_task(task.0) else {
            return self.refuse(Violation::UnknownTask);
        };
        let ghost view = self@;
        proof {
            // Task seqs are distinct: `at` is the only task `task` names.
            assert forall|other: int| 0 <= other < view.tasks.len() && (#[trigger] view.tasks[other]).seq
                == task.0 implies other == at by {
                if other < at {
                    assert(view.tasks[other].seq < view.tasks[at as int].seq);
                } else if other > at {
                    assert(view.tasks[at as int].seq < view.tasks[other].seq);
                }
            }
        };
        let synthesized_fits = matches!(exit, Exit::Unknown) || (matches!(exit, Exit::NotStarted(_))
            && matches!(self.tasks[at].class, Class::Safe));
        if output.is_none() && !synthesized_fits {
            return self.refuse(Violation::WrongResult);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::TaskEnded { task, exit, output: *output });
        };
        let ended = Task { ended: true, ..self.tasks[at] };
        self.tasks.set(at, ended);
        let bytes = match output {
            Some(output) => output.bytes,
            None => 0,
        };
        self.mailbox.push(Waiting { id: EnvelopeId::Task(task), user: false, bytes });
        self.seq = next;
        assert(self@.tasks =~= end_task(view.tasks, task.0));
        Ok(())
    }

    /// `apply` of `StopSent { child }`.
    fn apply_stop_sent(&mut self, child: ActorId) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::StopSent { child }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        if !self.stopped || !self.spawned(child) || self.stopping(child) {
            return self.refuse(Violation::WrongStop);
        }
        proof {
            lemma_actor_keeps(self@, Record::<P>::StopSent { child });
        };
        self.tasks.push(
            Task { seq: self.seq, class: Class::Keyed, origin: Origin::Stop { child }, ended: false, cancelled: false },
        );
        self.runs.push(None);
        self.seq = next;
        Ok(())
    }

    /// Applies one durable record. The only way state changes; runs only
    /// after the log acknowledged `record`.
    ///
    /// # Errors
    ///
    /// `Corrupt` when `record` cannot follow the log so far; the core is
    /// left unchanged.
    #[verifier::spinoff_prover]
    pub fn apply(&mut self, record: &Record<P>) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, *record) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
            applied is Ok ==> final(self)@.seq == old(self)@.seq + 1,
    {
        let next = bump(self.seq);
        proof {
            if spec_apply(self@, *record) is Some {
                lemma_actor_keeps(self@, *record);
            }
        };
        match record {
            Record::Born { born } => {
                if self.born.is_some() {
                    return self.refuse(Violation::Reborn);
                }
                self.born = Some(born.copy());
                self.seq = next;
                Ok(())
            },
            Record::RulesLoaded { rules, .. } => {
                self.rules = Some((self.seq, rules.copy()));
                self.seq = next;
                Ok(())
            },
            Record::Delivered { envelope } => self.apply_delivered(envelope),
            Record::TurnStarted { turn } => self.apply_turn_started(*turn),
            Record::StepStarted { step, carries } => self.apply_step_started(*step, carries),
            Record::ModelOutput { step, parts } => {
                let open = self.open()?;
                if !(matches!(open.stage, Stage::Model { .. })) {
                    return self.refuse(Violation::WrongStage);
                }
                if !step.same(open.step) {
                    return self.refuse(Violation::WrongStep);
                }
                self.context = self.context.grow(parts.bytes, self.limits.context_bytes());
                self.seq = next;
                Ok(())
            },
            Record::StepReset { step, reason } => self.apply_step_reset(*step, *reason),
            Record::StepFinished { step, calls } => self.apply_step_finished(*step, calls),
            Record::Approved { call, by } => self.apply_approved(*call, *by),
            Record::Dispatched { call } => self.apply_dispatched(*call),
            Record::ToolResult { call, exit, output } => self.apply_tool_result(*call, *exit, output),
            Record::TaskEnded { task, exit, output } => self.apply_task_ended(*task, *exit, output),
            Record::StopSent { child } => self.apply_stop_sent(*child),
            Record::CancelRequested { turn } => {
                let open = self.open()?;
                if !turn.same(open.turn) {
                    return self.refuse(Violation::WrongTurn);
                }
                if matches!(open.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
                }
                self.turn = Turn::Open(Open { intent: Intent::Cancel, ..open });
                self.seq = next;
                Ok(())
            },
            Record::TurnEnded { turn, outcome } => {
                let open = self.open()?;
                if !turn.same(open.turn) {
                    return self.refuse(Violation::WrongTurn);
                }
                let fits = match (open.intent, outcome) {
                    (Intent::Cancel, TurnOutcome::Cancelled)
                    | (Intent::Proceed, TurnOutcome::Completed | TurnOutcome::Failed(_)) => true,
                    (Intent::Cancel, TurnOutcome::Completed | TurnOutcome::Failed(_))
                    | (Intent::Proceed, TurnOutcome::Cancelled) => false,
                };
                if !fits {
                    return self.refuse(Violation::WrongEnding);
                }
                if !self.all_resolved(0) {
                    return self.refuse(Violation::Unresolved);
                }
                self.parked = matches!(open.stage, Stage::Compacting { .. }) || matches!(
                    outcome,
                    TurnOutcome::Failed(Failure::Limit(Limit::Context)),
                );
                self.compacted = false;
                self.turn = Turn::Idle;
                self.seq = next;
                Ok(())
            },
            Record::EpochBumped { epoch } => {
                match self.epoch.0.checked_add(1) {
                    Some(bumped) => {
                        if bumped != epoch.0 {
                            return self.refuse(Violation::Epoch);
                        }
                    },
                    None => return self.refuse(Violation::Epoch),
                }
                self.epoch = *epoch;
                self.seq = next;
                Ok(())
            },
            Record::CompactionStarted { through } => self.apply_compaction_started(*through),
            Record::Compacted { started, summary, open } => self.apply_compacted(*started, summary, open),
        }
    }

    /// `apply` of `Compacted { started, summary, open }`. The conversation
    /// starts over from the summary and the open work.
    const fn apply_compacted(&mut self, started: u64, summary: &Measured<P::Text>, open: &OpenWork) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::Compacted { started, summary: *summary, open: *open }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        match self.turn {
            Turn::Idle => {
                let live = match self.compacting {
                    Some(seq) => seq == started,
                    None => false,
                };
                if !live {
                    return self.refuse(Violation::NotCompacting);
                }
                proof {
                    lemma_actor_keeps(self@, Record::<P>::Compacted { started, summary: *summary, open: *open });
                };
                self.compacting = None;
                self.parked = false;
            },
            Turn::Open(running) => {
                let (since, then) = match running.stage {
                    Stage::Compacting { since, then } => (since, then),
                    Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Tools => {
                        return self.refuse(Violation::NotCompacting);
                    },
                };
                if since != started {
                    return self.refuse(Violation::NotCompacting);
                }
                if matches!(running.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
                }
                proof {
                    lemma_actor_keeps(self@, Record::<P>::Compacted { started, summary: *summary, open: *open });
                };
                self.turn = Turn::Open(Open { stage: then.stage(), ..running });
                self.compacted = true;
            },
        }
        let entries = open.tasks.len().saturating_add(open.children.len());
        self.context = Context::restart(summary.bytes, entries, self.limits.context_bytes());
        self.step_base = self.context;
        self.seq = next;
        Ok(())
    }

    /// `apply` of `StepReset { step, reason }`: the attempt's output no
    /// longer counts.
    fn apply_step_reset(&mut self, step: StepId, reason: Reset) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::StepReset { step, reason }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        let open = self.open()?;
        if !(matches!(open.stage, Stage::Model { .. })) {
            return self.refuse(Violation::WrongStage);
        }
        if matches!(open.intent, Intent::Cancel) {
            return self.refuse(Violation::Cancelling);
        }
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        if !step.same(open.step) {
            return self.refuse(Violation::WrongStep);
        }
        let retries = match reason {
            Reset::Retry { .. } => {
                if open.retries == usize::MAX {
                    return self.refuse(Violation::Overflow);
                }
                open.retries + 1
            },
            Reset::Restart => open.retries,
        };
        proof {
            lemma_actor_keeps(self@, Record::<P>::StepReset { step, reason });
        };
        self.turn = Turn::Open(
            Open { retries, stage: Stage::Backoff { since: self.seq, reset: reason }, ..open },
        );
        self.context = self.step_base;
        self.seq = next;
        Ok(())
    }

    /// `apply` of `CompactionStarted { through }`.
    fn apply_compaction_started(&mut self, through: u64) -> (applied: Result<(), Corrupt>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match spec_apply(old(self)@, Record::<P>::CompactionStarted { through }) {
                Some(next) => applied is Ok && final(self)@ == next,
                None => applied is Err && final(self)@ == old(self)@,
            },
    {
        let next = bump(self.seq);
        if through != self.seq {
            return self.refuse(Violation::WrongCut);
        }
        if self.stopped {
            return self.refuse(Violation::Stopped);
        }
        match self.turn {
            Turn::Idle => {
                if self.born.is_none() {
                    return self.refuse(Violation::Unborn);
                }
                proof {
                    lemma_actor_keeps(self@, Record::<P>::CompactionStarted { through });
                };
                self.compacting = Some(self.seq);
            },
            Turn::Open(open) => {
                let then = match open.stage {
                    Stage::Starting => Then::Starting,
                    Stage::Tools => Then::Tools,
                    Stage::Compacting { then, .. } => then,
                    Stage::Model { .. } | Stage::Backoff { .. } => return self.refuse(Violation::WrongStage),
                };
                if matches!(open.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
                }
                if !self.all_resolved(0) {
                    return self.refuse(Violation::Unresolved);
                }
                proof {
                    lemma_actor_keeps(self@, Record::<P>::CompactionStarted { through });
                };
                self.turn = Turn::Open(Open { stage: Stage::Compacting { since: self.seq, then }, ..open });
            },
        }
        self.seq = next;
        Ok(())
    }

    /// Rebuilds the state a log describes by applying it record by record.
    ///
    /// # Errors
    ///
    /// The first record `apply` refuses: recovery stops there.
    pub fn recover(limits: Limits, log: &[Record<P>]) -> (recovered: Result<Core<P>, Corrupt>)
        ensures
            match recovered {
                Ok(core) => core.wf() && replay(log@) == Some(core@) && state_ok(core@),
                Err(_) => replay(log@) is None,
            },
    {
        let mut core = Core::new(limits);
        let mut index: usize = 0;
        while index < log.len()
            invariant
                index <= log@.len(),
                core.wf(),
                applies(initial(), log@.take(index as int)) == Some(core@),
            decreases log@.len() - index,
        {
            assert(log@.take(index + 1).drop_last() == log@.take(index as int));
            match core.apply(&log[index]) {
                Ok(()) => {},
                Err(corrupt) => {
                    proof {
                        lemma_refusal_sticks(initial(), log@, index + 1);
                    };
                    return Err(corrupt);
                },
            }
            index += 1;
        }
        assert(log@.take(log@.len() as int) == log@);
        Ok(core)
    }

    // -----------------------------------------------------------------------
    // decide
    // -----------------------------------------------------------------------

    fn plan(&self) -> (plan: Plan<P>)
        requires
            self.wf(),
        ensures
            plan.inv(),
            plan.start@ == self@,
            plan.scratch@ == self@,
            plan.records@.len() == 0,
            plan.given@ is None,
    {
        let plan = Plan {
            start: Ghost(self@),
            from: self.seq,
            given: Ghost(None),
            scratch: self.scratch(),
            records: Vec::new(),
            effects: Vec::new(),
        };
        assert(plan.records@.len() == 0);
        plan
    }

    /// Starts model call `index` of `turn`, carrying the batch at the head
    /// of the mailbox (nothing, for a retry), unless a bound says the turn
    /// is over. The index is widened, so the step after the last `u32` is
    /// past the bound too. When the step would not fit the context, a beat
    /// (not a retry, and not one that compacted already: `Core::compacted`)
    /// compacts the history instead; the step starts once it is written.
    fn start_step(&self, plan: &mut Plan<P>, turn: TurnId, index: u64)
        requires
            old(plan).inv(),
            may_start(old(plan).scratch@, turn, index as int),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
    {
        let Some(open) = plan.open() else {
            return;
        };
        let count = match open.stage {
            Stage::Backoff { .. } => 0,
            Stage::Starting | Stage::Model { .. } | Stage::Tools | Stage::Compacting { .. } => mailbox::batch(
                plan.scratch.mailbox.as_slice(),
            ),
        };
        proof {
            if !(open.stage is Backoff) {
                assert(count <= plan.scratch@.mailbox.len());
            }
        };
        let (carries, bytes) = mailbox::head(plan.scratch.mailbox.as_slice(), count);
        if index >= u64::from(self.limits.steps_per_turn()) {
            plan.emit(Record::TurnEnded { turn, outcome: TurnOutcome::Failed(Failure::Limit(Limit::Steps)) });
        } else if matches!(plan.scratch.context.grow(bytes, self.limits.context_bytes()), Context::Exceeded) {
            if !plan.scratch.compacted && matches!(open.stage, Stage::Starting | Stage::Tools) {
                plan.emit(Record::CompactionStarted { through: plan.scratch.seq });
            } else {
                plan.emit(Record::TurnEnded { turn, outcome: TurnOutcome::Failed(Failure::Limit(Limit::Context)) });
            }
        } else {
            #[expect(clippy::as_conversions, clippy::cast_possible_truncation, reason = "Verus proves the index is below steps_per_turn, a u32")]
            let step = StepId { turn, index: index as u32 };
            let seq = plan.emit(Record::StepStarted { step, carries });
            let id = plan.id(seq);
            plan.effect(Effect::CallModel { id, step });
        }
    }

    /// Starts a turn when the actor is idle, live, not parked and has mail,
    /// within `Limits::turns_per_actor`; with no mail, compacts after a
    /// turn this decision ended (`after_turn`). Runs once per decision, so
    /// a turn the context bound fails at once cannot loop. A turn
    /// supersedes a compaction between turns and cancels its summarize
    /// call.
    fn begin(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
    {
        if plan.open().is_some() || plan.scratch.live().is_none() || plan.scratch.parked {
            return;
        }
        if plan.scratch.mailbox.len() == 0 {
            self.after_turn(plan);
            return;
        }
        #[expect(clippy::as_conversions, reason = "a usize fits in a u64 on every target")]
        let limit = self.limits.turns_per_actor() as u64;
        if plan.scratch.turns >= limit {
            return;
        }
        if let Some(since) = plan.scratch.compacting {
            let id = plan.id(since);
            plan.effect(Effect::CancelModel { id });
        }
        let turn = TurnId(plan.scratch.seq);
        plan.emit(Record::TurnStarted { turn });
        assert(resolved(plan.scratch@.calls, 0, plan.scratch@.calls.len() as int));
        self.start_step(plan, turn, 0);
    }

    /// Compaction at the end of a turn, for `begin`: when this decision
    /// ended a turn and left the conversation past 80% of `context_bytes`,
    /// the idle actor compacts its history now.
    fn after_turn(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.turn is Idle,
            live(old(plan).scratch@),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
    {
        if !plan.ended() {
            return;
        }
        if plan.scratch.context.over(self.limits.context_bytes()) {
            plan.emit(Record::CompactionStarted { through: plan.scratch.seq });
        }
    }

    /// Moves the current step's calls forward: cancels and stops, calls
    /// past a bound, invalid requests, rule grants, dispatch of every
    /// approved call, and once every call is resolved, the next step or
    /// the end of the turn.
    #[verifier::loop_isolation(false)]
    fn settle(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.turn is Open,
            old(plan).scratch@.turn->Open_0.stage is Tools,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
    {
        let Some(open) = plan.open() else {
            return;
        };
        let count = plan.scratch.calls.len();
        let stopped = plan.scratch.stopped;
        // A turn being cancelled, or an actor stopped, starts nothing more:
        // the gate closes (C3).
        let closing = stopped || matches!(open.intent, Intent::Cancel);
        let gate = if stopped {
            Unstarted::Stopped
        } else {
            Unstarted::Cancelled
        };
        // Steering: while a user's message waits, no call waits for the
        // user; each is denied, and the message lands at the boundary.
        let steered = mailbox::steers(plan.scratch.mailbox.as_slice());
        let depth = match &plan.scratch.born {
            Some(born) => born.depth,
            None => 0,
        };
        let mut index = open.first_call;
        while index < count
            invariant
                plan.inv(),
                plan.start == old(plan).start,
                plan.given == old(plan).given,
                plan.from == old(plan).from,
                plan.scratch@.epoch == old(plan).scratch@.epoch,
                plan.scratch@.stopped == stopped,
                plan.scratch@.turn == Turn::Open(open),
                plan.scratch@.calls.len() == count,
                open.first_call <= index,
            decreases count - index,
        {
            assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
            match plan.scratch.calls[index].phase {
                Phase::Proposed => {
                    if closing {
                        plan.resolve(index, Exit::NotStarted(gate));
                    } else if index - open.first_call >= self.limits.calls_per_step() || index
                        >= self.limits.calls_per_turn() || (plan.scratch.calls[index].spawns() && depth
                        >= self.limits.depth()) {
                        plan.resolve(index, Exit::NotStarted(Unstarted::Limit));
                    } else {
                        match plan.scratch.rule_for(index) {
                            Granted::Invalid => plan.resolve(index, Exit::NotStarted(Unstarted::Invalid)),
                            Granted::Rule(by) => {
                                plan.emit(Record::Approved { call: CallIx(index), by });
                            },
                            Granted::Ask => {
                                if steered {
                                    plan.resolve(index, Exit::NotStarted(Unstarted::Steered));
                                }
                            },
                        }
                    }
                },
                Phase::Approved => {
                    if closing {
                        plan.resolve(index, Exit::NotStarted(gate));
                    }
                },
                Phase::Dispatched { .. } | Phase::Resolved => {},
            }
            index += 1;
        }
        // Dispatch every approved call, in proposal order. The core has no
        // concurrency rules: what runs together is the tools' business.
        if !closing {
            let mut index = open.first_call;
            while index < count
                invariant
                    plan.inv(),
                    plan.start == old(plan).start,
                    plan.given == old(plan).given,
                    plan.from == old(plan).from,
                    plan.scratch@.epoch == old(plan).scratch@.epoch,
                    plan.scratch@.stopped == stopped,
                    !stopped,
                    plan.scratch@.turn == Turn::Open(open),
                    plan.scratch@.calls.len() == count,
                    open.first_call <= index,
                    open.intent is Proceed,
                decreases count - index,
            {
                assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
                if matches!(plan.scratch.calls[index].phase, Phase::Approved) {
                    self.dispatch(plan, index);
                }
                index += 1;
            }
        }
        // Once every call is resolved: the turn ends or the next step starts.
        if plan.scratch.all_resolved(open.first_call) {
            proof {
                let view = plan.scratch@;
                assert forall|at: int| 0 <= at < view.calls.len() implies (#[trigger] view.calls[at]).phase is Resolved by {
                    if at < open.first_call {
                        assert(resolved(view.calls, 0, open.first_call as int));
                    }
                }
            };
            match open.intent {
                Intent::Cancel => {
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
                },
                Intent::Proceed => {
                    if stopped {
                        // A torn stop: `resume` cancels the turn first, so
                        // no decision reaches here.
                    } else if count == open.first_call {
                        plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Completed });
                    } else {
                        self.start_step(plan, open.turn, u64::from(open.step.index) + 1);
                    }
                },
            }
        }
    }

    /// Dispatches approved call `index`. A background call past
    /// `Limits::tasks_per_actor` does not start, nor a `spawn` past
    /// `Limits::children_per_actor`. Once dispatched, a
    /// background call resolves `Ok` (its task runs on: no turn waits for
    /// it, P1), and a `cancel` is done at once by the core.
    fn dispatch(&self, plan: &mut Plan<P>, index: usize)
        requires
            old(plan).inv(),
            !old(plan).scratch@.stopped,
            old(plan).scratch@.turn is Open,
            old(plan).scratch@.turn->Open_0.stage is Tools,
            old(plan).scratch@.turn->Open_0.intent is Proceed,
            old(plan).scratch@.turn->Open_0.first_call <= index < old(plan).scratch@.calls.len(),
            old(plan).scratch@.calls[index as int].phase is Approved,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
            final(plan).scratch@.turn == old(plan).scratch@.turn,
            final(plan).scratch@.calls.len() == old(plan).scratch@.calls.len(),
    {
        assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
        assert(call_ok(plan.scratch@.calls[index as int]));
        let (background, cancel) = match &plan.scratch.calls[index].shape {
            Some(shape) => (shape.background(), shape.task()),
            None => (false, None),
        };
        if background && plan.scratch.tasks.len() >= self.limits.tasks_per_actor() {
            plan.resolve(index, Exit::NotStarted(Unstarted::Limit));
            return;
        }
        if plan.scratch.calls[index].spawns() && plan.scratch.children.len() >= self.limits.children_per_actor() {
            plan.resolve(index, Exit::NotStarted(Unstarted::Limit));
            return;
        }
        // Whether an earlier cancel signalled the task: this `Dispatched`
        // marks it so.
        let signalled = match cancel {
            Some(task) => plan.scratch.signalled(task.0),
            None => false,
        };
        plan.emit(Record::Dispatched { call: CallIx(index) });
        if background {
            plan.resolve(index, Exit::Ok);
        } else if let Some(task) = cancel {
            let hit = Self::cancel_task(plan, task, signalled);
            plan.resolve(index, if hit { Exit::Ok } else { Exit::Failed });
        }
    }

    /// The cancel primitive on task `task`: a running `Safe` one (a timer
    /// that has not fired) ends `NotStarted`; a running `Unsafe` one is
    /// signalled, unless it was (`signalled`), and records what comes back.
    /// A stop is not cancelled. Whether anything was cancelled.
    fn cancel_task(plan: &mut Plan<P>, task: TaskId, signalled: bool) -> (hit: bool)
        requires
            old(plan).inv(),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
            final(plan).scratch@.turn == old(plan).scratch@.turn,
            final(plan).scratch@.calls == old(plan).scratch@.calls,
    {
        let Some(at) = plan.scratch.running_task(task.0) else {
            return false;
        };
        match plan.scratch.tasks[at].class {
            Class::Safe => {
                plan.end_task(at, Exit::NotStarted(Unstarted::Cancelled));
                true
            },
            Class::Unsafe => {
                if !signalled {
                    let id = plan.id(task.0);
                    plan.effect(Effect::CancelTool { id });
                }
                true
            },
            Class::Keyed => false,
        }
    }

    /// Steering: a user message arrived while the step may wait for
    /// approvals. `settle` denies each call still awaiting the user ("the
    /// user steered"), and the message lands at the step's boundary, once
    /// every call has resolved.
    fn steer(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
    {
        let Some(open) = plan.open() else {
            return;
        };
        if matches!(open.stage, Stage::Tools) && matches!(open.intent, Intent::Proceed) && !plan.scratch.stopped {
            self.settle(plan);
        }
    }

    /// Revokes the running turn `turn`, whose cancel was just recorded:
    /// nothing more starts, the model call or summarize call in flight is
    /// stopped, each
    /// dispatched call is signalled once, and the turn ends once every call
    /// has resolved.
    #[verifier::loop_isolation(false)]
    fn revoke(&self, plan: &mut Plan<P>, turn: TurnId)
        requires
            old(plan).inv(),
            cancelled(old(plan).scratch@, turn),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
    {
        let Some(open) = plan.open() else {
            return;
        };
        match open.stage {
            Stage::Starting | Stage::Backoff { .. } => {
                plan.emit(Record::TurnEnded { turn, outcome: TurnOutcome::Cancelled });
            },
            Stage::Model { since } | Stage::Compacting { since, .. } => {
                let id = plan.id(since);
                plan.effect(Effect::CancelModel { id });
            },
            Stage::Tools => {
                let count = plan.scratch.calls.len();
                let mut index = open.first_call;
                while index < count
                    invariant
                        plan.inv(),
                        plan.start == old(plan).start,
                        plan.given == old(plan).given,
                        plan.from == old(plan).from,
                        plan.scratch@ == old(plan).scratch@,
                        plan.scratch@.turn == Turn::Open(open),
                        plan.scratch@.calls.len() == count,
                    decreases count - index,
                {
                    if let Phase::Dispatched { seq } = plan.scratch.calls[index].phase {
                        let id = plan.id(seq);
                        plan.effect(Effect::CancelTool { id });
                    }
                    index += 1;
                }
                self.settle(plan);
            },
        }
    }

    /// A stop's part for the actor's tasks: a running `Safe` one (a timer)
    /// ends `NotStarted`, a running `Unsafe` one is signalled unless a
    /// cancel signalled it, and a stop already on its way is left to arrive.
    #[verifier::loop_isolation(false)]
    fn stop_tasks(plan: &mut Plan<P>)
        requires
            old(plan).inv(),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
    {
        let count = plan.scratch.tasks.len();
        let mut at: usize = 0;
        while at < count
            invariant
                plan.inv(),
                plan.start == old(plan).start,
                plan.given == old(plan).given,
                plan.from == old(plan).from,
                plan.scratch@.epoch == old(plan).scratch@.epoch,
                plan.scratch@.stopped == old(plan).scratch@.stopped,
                plan.scratch@.tasks.len() == count,
            decreases count - at,
        {
            let task = plan.scratch.tasks[at];
            if !task.ended {
                match task.class {
                    Class::Safe => plan.end_task(at, Exit::NotStarted(Unstarted::Stopped)),
                    Class::Unsafe => {
                        if !task.cancelled {
                            let id = plan.id(task.seq);
                            plan.effect(Effect::CancelTool { id });
                        }
                    },
                    Class::Keyed => {},
                }
            }
            at += 1;
        }
    }

    /// The stop cascade: a stop for every actor this stopped one spawned
    /// that has none yet. Each is a `Keyed` task, started by its
    /// `StopSent`.
    #[verifier::loop_isolation(false)]
    fn stop_children(plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.stopped,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped,
    {
        let count = plan.scratch.children.len();
        let mut at: usize = 0;
        while at < count
            invariant
                plan.inv(),
                plan.start == old(plan).start,
                plan.given == old(plan).given,
                plan.from == old(plan).from,
                plan.scratch@.epoch == old(plan).scratch@.epoch,
                plan.scratch@.stopped,
                plan.scratch@.children == old(plan).scratch@.children,
                count == plan.scratch@.children.len(),
            decreases count - at,
        {
            let child = plan.scratch.children[at];
            if !plan.scratch.stopping(child) {
                assert(plan.scratch@.children[at as int] == child);
                plan.emit(Record::StopSent { child });
            }
            at += 1;
        }
    }

    /// A stop was just delivered: revoke the running turn (or stop a
    /// compaction between turns), the tasks, and stop every child (C3).
    fn cascade(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.stopped,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
    {
        match plan.open() {
            Some(open) => {
                if matches!(open.intent, Intent::Proceed) {
                    plan.emit(Record::CancelRequested { turn: open.turn });
                    self.revoke(plan, open.turn);
                }
            },
            None => {
                if let Some(since) = plan.scratch.compacting {
                    let id = plan.id(since);
                    plan.effect(Effect::CancelModel { id });
                }
            },
        }
        Self::stop_tasks(plan);
        Self::stop_children(plan);
    }

    /// Decides what an event means: records to persist and effects to start
    /// once they are durable. Mutates nothing. The records apply to this
    /// state one after another, and every effect is licensed by them.
    pub fn decide(&self, event: Event<P>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(event, decision.records@),
    {
        match event {
            Event::Born { born } => self.on_born(born),
            Event::Rules { rules, file } => self.on_rules(rules, file),
            Event::Deliver { envelope } => self.on_deliver(envelope),
            Event::Approve { call, verdict } => self.on_approve(&call, verdict),
            Event::Interrupt { turn } => self.on_interrupt(turn),
            Event::ModelParts { id, parts } => self.on_parts(id, parts),
            Event::ModelDone { id, result } => self.on_model_done(id, result),
            Event::ToolDone { id, exit, output } => self.on_tool_done(id, exit, output),
            Event::Woke { id } => self.on_woke(id),
            Event::Expired { id } => self.on_expired(id),
            Event::Compact => self.on_compact(),
            Event::Summarized { id, summary } => self.on_summarized(id, summary),
        }
    }

    /// The user's `/compact`: compact the history between turns. Refused
    /// while a turn runs: its beats compact it when they must. A repeat
    /// starts over under a new cut and cancels the summarize call before.
    fn on_compact(&self) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Compact, decision.records@),
    {
        if self.born.is_none() {
            return quiet(Outcome::Rejected(Rejection::Unborn));
        }
        if self.stopped {
            return quiet(Outcome::Rejected(Rejection::Stopped));
        }
        if matches!(self.turn, Turn::Open(_)) {
            return quiet(Outcome::Rejected(Rejection::Running));
        }
        let mut plan = self.plan();
        if let Some(since) = plan.scratch.compacting {
            let id = plan.id(since);
            plan.effect(Effect::CancelModel { id });
        }
        plan.emit(Record::CompactionStarted { through: plan.scratch.seq });
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// A summarize call ended. In a turn, its summary is recorded and the
    /// step it made room for starts. A summary that still leaves the step
    /// too big, or a failed one, fails the turn at the full context, and
    /// one under a cancel ends it `Cancelled`: in each of those three cases
    /// the actor parks (`Core::parked`). Between turns, a summary is
    /// recorded and unparks the actor, and a failure changes nothing. A
    /// compaction a turn superseded, or one before a crash, is stale.
    fn on_summarized(&self, id: EffectId, result: Summary<P>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Summarized { id, summary: result }, decision.records@),
    {
        let mut plan = self.plan();
        if let Some(open) = plan.open() {
            let (since, then) = match open.stage {
                Stage::Compacting { since, then } => (since, then),
                Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Tools => {
                    return quiet(Outcome::Stale);
                },
            };
            if !id.same(EffectId { epoch: self.epoch, seq: since }) {
                return quiet(Outcome::Stale);
            }
            match open.intent {
                Intent::Cancel => {
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
                },
                Intent::Proceed => {
                    if plan.scratch.stopped {
                        // A torn stop: `resume` cancels the turn first, so
                        // no decision reaches here.
                        return quiet(Outcome::Stale);
                    }
                    match result {
                        Summary::Written(summary) => {
                            let open_work = plan.scratch.open_work();
                            plan.emit(Record::Compacted { started: since, summary, open: open_work });
                            let index = match then {
                                Then::Starting => 0,
                                Then::Tools => u64::from(open.step.index) + 1,
                            };
                            self.start_step(&mut plan, open.turn, index);
                        },
                        Summary::Failed(_) => {
                            plan.emit(
                                Record::TurnEnded {
                                    turn: open.turn,
                                    outcome: TurnOutcome::Failed(Failure::Limit(Limit::Context)),
                                },
                            );
                        },
                    }
                },
            }
        } else {
            let live = match plan.scratch.compacting {
                Some(seq) => id.same(EffectId { epoch: self.epoch, seq }),
                None => false,
            };
            if !live || plan.scratch.live().is_none() {
                return quiet(Outcome::Stale);
            }
            match result {
                Summary::Written(summary) => {
                    let open_work = plan.scratch.open_work();
                    plan.emit(Record::Compacted { started: id.seq, summary, open: open_work });
                    self.begin(&mut plan);
                },
                Summary::Failed(_) => return quiet(Outcome::Accepted),
            }
        }
        plan.finish(Outcome::Accepted, Runs::New)
    }

    fn on_born(&self, born: Born) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Born { born }, decision.records@),
    {
        if let Some(already) = &self.born {
            return quiet(
                if already.actor.same(born.actor) {
                    Outcome::Exists
                } else {
                    Outcome::Rejected(Rejection::Mismatch)
                },
            );
        }
        if born.depth > self.limits.depth() {
            return quiet(Outcome::Rejected(Rejection::Limit));
        }
        let mut plan = self.plan();
        plan.emit(Record::Born { born });
        plan.finish(Outcome::Accepted, Runs::New)
    }

    fn on_rules(&self, rules: Rules, file: Option<Measured<P::Text>>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Rules { rules, file }, decision.records@),
    {
        let mut plan = self.plan();
        plan.emit(Record::RulesLoaded { rules, file });
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// An envelope arrives. A repeat is `Exists` and records nothing (C2).
    /// It lands in the mailbox, and a model call consumes it at the next
    /// step boundary (T4): it starts a turn if the actor is idle, and
    /// a user's message steers a step waiting for approvals. A stop revokes
    /// everything (C3).
    fn on_deliver(&self, envelope: Envelope<P>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Deliver { envelope }, decision.records@),
    {
        let Some(born) = &self.born else {
            return quiet(Outcome::Rejected(Rejection::Unborn));
        };
        if !born.actor.same(envelope.to) || matches!(envelope.id, EnvelopeId::Task(_)) {
            return quiet(Outcome::Rejected(Rejection::Mismatch));
        }
        if self.seen(envelope.id) {
            return quiet(Outcome::Exists);
        }
        if self.stopped {
            return quiet(Outcome::Rejected(Rejection::Stopped));
        }
        // The limit bounds mail, never the stop that ends a runaway actor
        // (C3): only one stop ever lands.
        let stop = matches!(envelope.body, Body::Stop);
        if !stop && self.envelopes.len() >= self.limits.messages_per_actor() {
            return quiet(Outcome::Rejected(Rejection::Limit));
        }
        let user = matches!(envelope.from, Address::User);
        let mut plan = self.plan();
        plan.emit(Record::Delivered { envelope });
        if stop {
            self.cascade(&mut plan);
        } else if user {
            self.steer(&mut plan);
        }
        self.begin(&mut plan);
        plan.finish(Outcome::Accepted, Runs::New)
    }

    fn on_approve(&self, call: &CallId, verdict: Verdict) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Approve { call: *call, verdict }, decision.records@),
    {
        let mut plan = self.plan();
        let Some(open) = plan.open() else {
            return quiet(Outcome::Rejected(Rejection::UnknownCall));
        };
        if !(matches!(open.stage, Stage::Tools)) || plan.scratch.stopped {
            return quiet(Outcome::Rejected(Rejection::UnknownCall));
        }
        let count = plan.scratch.calls.len();
        let mut found: Option<usize> = None;
        let mut index = open.first_call;
        while index < count
            invariant
                count == plan.scratch.calls@.len(),
                open.first_call <= index,
                match found {
                    Some(at) => open.first_call <= at < count,
                    None => true,
                },
            decreases count - index,
        {
            if found.is_none() && plan.scratch.calls[index].id.same(call) {
                found = Some(index);
            }
            index += 1;
        }
        let Some(index) = found else {
            return quiet(Outcome::Rejected(Rejection::UnknownCall));
        };
        assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
        // Only a valid call still proposed awaits approval, and none does
        // while the turn is being cancelled (the interrupt resolves it). A
        // repeated verdict finds nothing; the log tells it apart.
        if !(matches!(plan.scratch.calls[index].phase, Phase::Proposed)) || plan.scratch.calls[index].args.is_none()
            || matches!(open.intent, Intent::Cancel) {
            return quiet(Outcome::Rejected(Rejection::UnknownCall));
        }
        match verdict {
            Verdict::Approve => {
                // The one grant only your command gives (C1).
                plan.given = Ghost(Some(Record::Approved { call: CallIx(index), by: Grant::User }));
                plan.emit(Record::Approved { call: CallIx(index), by: Grant::User });
            },
            Verdict::Deny => plan.resolve(index, Exit::NotStarted(Unstarted::Denied)),
        }
        self.settle(&mut plan);
        self.begin(&mut plan);
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// Esc: cancel the running turn. Its tasks and children are left alone.
    fn on_interrupt(&self, turn: TurnId) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Interrupt { turn }, decision.records@),
    {
        let mut plan = self.plan();
        let Some(open) = plan.open() else {
            return quiet(Outcome::Rejected(Rejection::NotRunning));
        };
        if !open.turn.same(turn) {
            return quiet(Outcome::Rejected(Rejection::NotRunning));
        }
        if matches!(open.intent, Intent::Cancel) {
            return quiet(Outcome::Exists);
        }
        plan.emit(Record::CancelRequested { turn });
        self.revoke(&mut plan, turn);
        self.begin(&mut plan);
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// The running turn if model call `id` is the live one.
    const fn live_model(&self, id: EffectId) -> (live: Option<Open>)
        ensures
            match live {
                Some(open) => self@.turn == Turn::Open(open) && open.stage is Model,
                None => true,
            },
    {
        match self.turn {
            Turn::Open(open) => match open.stage {
                Stage::Model { since } => {
                    if id.same(EffectId { epoch: self.epoch, seq: since }) {
                        Some(open)
                    } else {
                        None
                    }
                },
                Stage::Starting | Stage::Backoff { .. } | Stage::Tools | Stage::Compacting { .. } => None,
            },
            Turn::Idle => None,
        }
    }

    fn on_parts(&self, id: EffectId, parts: Measured<P::Parts>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::ModelParts { id, parts }, decision.records@),
    {
        let Some(open) = self.live_model(id) else {
            return quiet(Outcome::Stale);
        };
        let mut plan = self.plan();
        plan.emit(Record::ModelOutput { step: open.step, parts });
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// Whether two of `calls` share an id.
    fn reused(calls: &[Proposal<P>]) -> bool {
        let mut reused = false;
        let mut first: usize = 0;
        while first < calls.len()
            decreases calls@.len() - first,
        {
            let mut second = first + 1;
            while second < calls.len()
                invariant
                    first < calls@.len(),
                decreases calls@.len() - second,
            {
                if calls[first].id.same(&calls[second].id) {
                    reused = true;
                }
                second += 1;
            }
            first += 1;
        }
        reused
    }

    #[expect(clippy::manual_map, reason = "Verus has no closures, so no `Option::map`")]
    fn on_model_done(&self, id: EffectId, result: ModelExit<P>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::ModelDone { id, result }, decision.records@),
    {
        let Some(open) = self.live_model(id) else {
            return quiet(Outcome::Stale);
        };
        let mut plan = self.plan();
        let failed = match open.intent {
            Intent::Proceed => TurnOutcome::Failed(Failure::Model(ModelFault::Other)),
            Intent::Cancel => TurnOutcome::Cancelled,
        };
        match result {
            ModelExit::Finished(calls) => {
                if Self::reused(&calls) {
                    // Never two live calls with one id: an approval finds
                    // its call by id.
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: failed });
                } else {
                    plan.emit(Record::StepFinished { step: open.step, calls });
                    self.settle(&mut plan);
                }
            },
            ModelExit::Failed(fault) => match open.intent {
                Intent::Cancel => {
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
                },
                Intent::Proceed => {
                    let retry = if retryable(fault) && !plan.scratch.stopped {
                        match self.limits.retry_delay(open.retries) {
                            Some(delay) => Some(Reset::Retry { fault, delay }),
                            None => None,
                        }
                    } else {
                        None
                    };
                    match retry {
                        Some(reason) => {
                            let seq = plan.emit(Record::StepReset { step: open.step, reason });
                            let id = plan.id(seq);
                            plan.effect(Effect::Sleep { id, delay: backoff(reason) });
                        },
                        None => {
                            plan.emit(
                                Record::TurnEnded {
                                    turn: open.turn,
                                    outcome: TurnOutcome::Failed(Failure::Model(fault)),
                                },
                            );
                        },
                    }
                },
            },
        }
        self.begin(&mut plan);
        plan.ask();
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// The current step's dispatched, unresolved call that tool run `id`
    /// runs, if `id` is the live one.
    fn running_call(plan: &Plan<P>, id: EffectId) -> (found: Option<usize>)
        requires
            plan.inv(),
        ensures
            match found {
                Some(index) => {
                    &&& plan.scratch@.turn is Open
                    &&& plan.scratch@.turn->Open_0.stage is Tools
                    &&& plan.scratch@.turn->Open_0.first_call <= index < plan.scratch@.calls.len()
                    &&& plan.scratch@.calls[index as int].phase is Dispatched
                },
                None => true,
            },
    {
        let open = plan.open()?;
        if !(matches!(open.stage, Stage::Tools)) || id.epoch.0 != plan.scratch.epoch.0 {
            return None;
        }
        let count = plan.scratch.calls.len();
        let mut found: Option<usize> = None;
        let mut index = open.first_call;
        while index < count
            invariant
                plan.inv(),
                count == plan.scratch.calls@.len(),
                open.first_call <= index,
                match found {
                    Some(at) => open.first_call <= at < count && plan.scratch@.calls[at as int].phase is Dispatched,
                    None => true,
                },
            decreases count - index,
        {
            assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
            let named = match plan.scratch.calls[index].phase {
                Phase::Dispatched { seq } => seq == id.seq,
                Phase::Proposed | Phase::Approved | Phase::Resolved => false,
            };
            if named {
                found = Some(index);
            }
            index += 1;
        }
        found
    }

    /// The running task that effect `id` runs, if `id` is the live one.
    fn task_of(plan: &Plan<P>, id: EffectId) -> (found: Option<usize>)
        requires
            plan.inv(),
        ensures
            match found {
                Some(at) => at < plan.scratch@.tasks.len() && plan.scratch@.tasks[at as int].seq == id.seq
                    && !plan.scratch@.tasks[at as int].ended,
                None => true,
            },
    {
        if id.epoch.0 != plan.scratch.epoch.0 {
            return None;
        }
        plan.scratch.running_task(id.seq)
    }

    /// A run reported: a call's, a task's or a delivery's. Whichever of
    /// this report and the run's deadline reaches the core first resolves
    /// it; the other finds it resolved and is stale (T3). A task's end is
    /// its notification, and may start a turn.
    fn on_tool_done(&self, id: EffectId, exit: Exit, output: Measured<P::Output>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::ToolDone { id, exit, output }, decision.records@),
    {
        let mut plan = self.plan();
        // The run's report, recorded exactly as it came (T3).
        if let Some(index) = Self::running_call(&plan, id) {
            plan.given = Ghost(Some(Record::ToolResult { call: CallIx(index), exit, output: Some(output) }));
            plan.emit(Record::ToolResult { call: CallIx(index), exit, output: Some(output) });
            self.settle(&mut plan);
        } else if Self::task_of(&plan, id).is_some() {
            proof {
                assert(running(plan.scratch@.tasks, id.seq));
            };
            plan.given = Ghost(Some(Record::TaskEnded { task: TaskId(id.seq), exit, output: Some(output) }));
            plan.emit(Record::TaskEnded { task: TaskId(id.seq), exit, output: Some(output) });
        } else {
            return quiet(Outcome::Stale);
        }
        self.begin(&mut plan);
        plan.finish(Outcome::Accepted, Runs::New)
    }

    /// A run's deadline passed: v stops waiting, records `Unknown` (it may
    /// have acted) and signals the run to stop, unless a cancel already did
    /// (Esc on its turn, a `cancel` of its task). A run that reported first
    /// keeps its outcome: the deadline is stale (T3).
    fn on_expired(&self, id: EffectId) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Expired { id }, decision.records@),
    {
        let mut plan = self.plan();
        if let Some(index) = Self::running_call(&plan, id) {
            let signalled = match plan.open() {
                Some(open) => matches!(open.intent, Intent::Cancel),
                None => false,
            };
            plan.resolve(index, Exit::Unknown);
            if !signalled {
                plan.effect(Effect::CancelTool { id });
            }
            self.settle(&mut plan);
        } else if let Some(at) = Self::task_of(&plan, id) {
            let signalled = plan.scratch.tasks[at].cancelled;
            plan.end_task(at, Exit::Unknown);
            if !signalled {
                plan.effect(Effect::CancelTool { id });
            }
        } else {
            return quiet(Outcome::Stale);
        }
        self.begin(&mut plan);
        plan.finish(Outcome::Accepted, Runs::New)
    }

    fn on_woke(&self, id: EffectId) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
            honest(Event::Woke { id }, decision.records@),
    {
        let mut plan = self.plan();
        let Some(open) = plan.open() else {
            return quiet(Outcome::Stale);
        };
        let since = match open.stage {
            Stage::Backoff { since, .. } => since,
            Stage::Starting | Stage::Model { .. } | Stage::Tools | Stage::Compacting { .. } => {
                return quiet(Outcome::Stale);
            },
        };
        if !id.same(EffectId { epoch: self.epoch, seq: since }) {
            return quiet(Outcome::Stale);
        }
        if matches!(open.intent, Intent::Proceed) && !plan.scratch.stopped {
            self.start_step(&mut plan, open.turn, u64::from(open.step.index));
        } else if matches!(open.intent, Intent::Cancel) {
            plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
        }
        self.begin(&mut plan);
        plan.finish(Outcome::Accepted, Runs::New)
    }

    // -----------------------------------------------------------------------
    // resume
    // -----------------------------------------------------------------------

    /// What to do after `recover`: bump the epoch so replies to effects from
    /// before the crash are stale, record who the actor is if it is not yet
    /// born (`born`, from the shell that made it) and the rules file if the
    /// shell read rules other than those in force (`load`), then pick up
    /// where the log stops. A stop the crash tore is finished first. A dispatched call
    /// with no result is rerun if it is `Safe` or `Keyed` and becomes
    /// `Unknown` if it is `Unsafe`; so is a running task. A model call in
    /// flight restarts its step; a backoff sleeps again; a compaction whose
    /// summary never came starts again. Like `decide`, the
    /// records apply and every effect is licensed by them.
    #[verifier::spinoff_prover]
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    pub fn resume(&self, born: Option<Born>, load: Option<Loaded<P>>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            resumed(self@, decision),
            nothing_given(decision.records@),
    {
        let mut plan = self.plan();
        let epoch = Epoch(bump(self.epoch.0));
        plan.emit(Record::EpochBumped { epoch });
        if let Some(born) = born {
            if plan.scratch.born.is_none() && born.depth <= self.limits.depth() {
                plan.emit(Record::Born { born });
            }
        }
        if let Some(Loaded { rules, file }) = load {
            plan.emit(Record::RulesLoaded { rules, file });
        }
        self.resume_turn(&mut plan);
        Self::resume_tasks(&mut plan);
        // A compaction between turns the crash cut short starts again,
        // unless mail waits for a turn that supersedes it.
        if plan.open().is_none() && plan.scratch.compacting.is_some() && plan.scratch.live().is_some()
            && (plan.scratch.mailbox.len() == 0 || plan.scratch.parked) {
            plan.emit(Record::CompactionStarted { through: plan.scratch.seq });
        }
        if plan.scratch.stopped {
            Self::stop_children(&mut plan);
        }
        self.begin(&mut plan);
        plan.ask();
        plan.finish(Outcome::Accepted, Runs::All)
    }

    /// `resume`'s part for the running turn: a stop the crash tore cancels
    /// it first; then it picks up at its stage.
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    fn resume_turn(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
    {
        if let Some(open) = plan.open() {
            if plan.scratch.stopped && matches!(open.intent, Intent::Proceed) {
                plan.emit(Record::CancelRequested { turn: open.turn });
            }
        }
        if let Some(open) = plan.open() {
            match (open.stage, open.intent) {
                (
                    Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } | Stage::Compacting { .. },
                    Intent::Cancel,
                ) => {
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
                },
                (Stage::Compacting { .. }, Intent::Proceed) => {
                    // A dangling start: compact again, under a new cut.
                    plan.emit(Record::CompactionStarted { through: plan.scratch.seq });
                },
                (Stage::Starting, Intent::Proceed) => {
                    self.start_step(plan, open.turn, 0);
                },
                (Stage::Model { .. }, Intent::Proceed) => {
                    plan.emit(Record::StepReset { step: open.step, reason: Reset::Restart });
                    self.start_step(plan, open.turn, u64::from(open.step.index));
                },
                (Stage::Backoff { since, reset }, Intent::Proceed) => {
                    let id = plan.id(since);
                    plan.effect(Effect::Sleep { id, delay: backoff(reset) });
                },
                (Stage::Tools, Intent::Proceed | Intent::Cancel) => {
                    Self::resume_tools(plan);
                    self.settle(plan);
                },
            }
        }
    }

    /// Resolves each dispatched call with no result that is not rerun: a
    /// background call's start (`Ok`: its task is running) and a `cancel`
    /// are the core's own and are done again; every `Unsafe` one is
    /// `Unknown`, and when the turn is being cancelled, the `Safe` and
    /// `Keyed` ones too, so no call is left dispatched under a cancel (C3:
    /// nothing reruns under it, and the turn still ends).
    #[verifier::loop_isolation(false)]
    fn resume_tools(plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.turn is Open,
            old(plan).scratch@.turn->Open_0.stage is Tools,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.turn == old(plan).scratch@.turn,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
            final(plan).scratch@.turn->Open_0.intent is Cancel ==> forall|index: int|
                0 <= index < final(plan).scratch@.calls.len() ==> !((
                #[trigger] final(plan).scratch@.calls[index]).phase is Dispatched),
    {
        let Some(open) = plan.open() else {
            return;
        };
        let count = plan.scratch.calls.len();
        let mut index = open.first_call;
        while index < count
            invariant
                plan.inv(),
                plan.start == old(plan).start,
                plan.given == old(plan).given,
                plan.from == old(plan).from,
                plan.scratch@.turn == Turn::Open(open),
                plan.scratch@.epoch == old(plan).scratch@.epoch,
                plan.scratch@.stopped == old(plan).scratch@.stopped,
                plan.scratch@.calls.len() == count,
                open.first_call <= index,
                resolved(plan.scratch@.calls, 0, open.first_call as int),
                open.intent is Cancel ==> forall|at: int|
                    open.first_call <= at < index ==> !((#[trigger] plan.scratch@.calls[at]).phase is Dispatched),
            decreases count - index,
        {
            assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
            if let Phase::Dispatched { .. } = plan.scratch.calls[index].phase {
                let (background, cancel) = match &plan.scratch.calls[index].shape {
                    Some(shape) => (shape.background(), shape.task()),
                    None => (false, None),
                };
                if background {
                    plan.resolve(index, Exit::Ok);
                } else if let Some(task) = cancel {
                    // Its durable `Dispatched` marked the task: a run in
                    // the old epoch is not signalled again.
                    let hit = Self::cancel_task(plan, task, true);
                    plan.resolve(index, if hit { Exit::Ok } else { Exit::Failed });
                } else {
                    // It was dispatched, so it may have acted: `NotStarted`
                    // would claim more than v knows (T3). A `Safe` or
                    // `Keyed` call is rerun only while the turn proceeds:
                    // rerunning it under a cancel would start something
                    // after the cancel is durable (C3).
                    match (plan.scratch.calls[index].class, open.intent) {
                        (None | Some(Class::Unsafe), Intent::Proceed | Intent::Cancel)
                        | (Some(Class::Safe | Class::Keyed), Intent::Cancel) => {
                            plan.resolve(index, Exit::Unknown);
                        },
                        (Some(Class::Safe | Class::Keyed), Intent::Proceed) => {},
                    }
                }
            }
            index += 1;
        }
    }

    /// Ends each running task that is not rerun: an `Unsafe` one is
    /// `Unknown` (it may have acted, and never starts again, C2), and a
    /// stopped actor's own ones never act again (C3: a timer `NotStarted`).
    /// The others run again (`finish` under `Runs::All`): a live actor's
    /// timers, and a stopped one's stops.
    #[verifier::loop_isolation(false)]
    fn resume_tasks(plan: &mut Plan<P>)
        requires
            old(plan).inv(),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).given == old(plan).given,
            final(plan).from == old(plan).from,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            final(plan).scratch@.stopped == old(plan).scratch@.stopped,
            final(plan).scratch@.stopped ==> forall|at: int|
                0 <= at < final(plan).scratch@.tasks.len() && (#[trigger] final(plan).scratch@.tasks[at]).origin is Call
                    ==> final(plan).scratch@.tasks[at].ended,
    {
        let stopped = plan.scratch.stopped;
        let count = plan.scratch.tasks.len();
        let mut at: usize = 0;
        while at < count
            invariant
                plan.inv(),
                plan.start == old(plan).start,
                plan.given == old(plan).given,
                plan.from == old(plan).from,
                plan.scratch@.epoch == old(plan).scratch@.epoch,
                plan.scratch@.stopped == stopped,
                plan.scratch@.tasks.len() == count,
                at <= count,
                stopped ==> forall|other: int|
                    0 <= other < at && (#[trigger] plan.scratch@.tasks[other]).origin is Call
                        ==> plan.scratch@.tasks[other].ended,
            decreases count - at,
        {
            let task = plan.scratch.tasks[at];
            let own = matches!(task.origin, Origin::Call { .. });
            if !task.ended {
                match task.class {
                    Class::Unsafe => plan.end_task(at, Exit::Unknown),
                    Class::Safe => {
                        if stopped && own {
                            plan.end_task(at, Exit::NotStarted(Unstarted::Stopped));
                        }
                    },
                    Class::Keyed => {
                        if stopped && own {
                            plan.end_task(at, Exit::Unknown);
                        }
                    },
                }
            }
            at += 1;
        }
    }
}

/// Whether model call `index` of `turn` may start from `view`: the next
/// `StepStarted` applies if the index is a `u32`, and so does a `TurnEnded`
/// failing the turn by a bound.
pub open spec fn may_start(view: CoreView, turn: TurnId, index: int) -> bool {
    match view.turn {
        Turn::Open(open) => {
            &&& open.intent is Proceed
            &&& !view.stopped
            &&& turn == open.turn
            &&& resolved(view.calls, 0, view.calls.len() as int)
            &&& match open.stage {
                Stage::Starting => index == 0,
                Stage::Backoff { .. } => index == open.step.index,
                Stage::Tools => index == open.step.index + 1,
                Stage::Model { .. } | Stage::Compacting { .. } => false,
            }
        },
        Turn::Idle => false,
    }
}

} // verus!

impl<P: Payload> fmt::Debug for Call<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call")
            .field("id", &self.id)
            .field("args", &self.args)
            .field("class", &self.class)
            .field("shape", &self.shape)
            .field("phase", &self.phase)
            .finish()
    }
}

impl<P: Payload> PartialEq for Call<P> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.args == other.args
            && self.class == other.class
            && self.shape == other.shape
            && self.phase == other.phase
    }
}

impl<P: Payload> fmt::Debug for TaskRun<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskRun")
            .field("id", &self.id)
            .field("args", &self.args)
            .finish()
    }
}

impl<P: Payload> PartialEq for TaskRun<P> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.args == other.args
    }
}

impl<P: Payload> fmt::Debug for Core<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("limits", &self.limits)
            .field("epoch", &self.epoch)
            .field("seq", &self.seq)
            .field("born", &self.born)
            .field("stopped", &self.stopped)
            .field("envelopes", &self.envelopes)
            .field("mailbox", &self.mailbox)
            .field("turns", &self.turns)
            .field("rules", &self.rules)
            .field("turn", &self.turn)
            .field("calls", &self.calls)
            .field("context", &self.context)
            .field("step_base", &self.step_base)
            .field("tasks", &self.tasks)
            .field("runs", &self.runs)
            .field("children", &self.children)
            .field("compacting", &self.compacting)
            .field("parked", &self.parked)
            .field("compacted", &self.compacted)
            .field("unborn", &self.unborn)
            .finish_non_exhaustive()
    }
}

/// Equal durable state. The ghost ledger is erased in plain builds.
impl<P: Payload> PartialEq for Core<P> {
    fn eq(&self, other: &Self) -> bool {
        self.limits == other.limits
            && self.epoch == other.epoch
            && self.seq == other.seq
            && self.born == other.born
            && self.stopped == other.stopped
            && self.envelopes == other.envelopes
            && self.mailbox == other.mailbox
            && self.turns == other.turns
            && self.rules == other.rules
            && self.turn == other.turn
            && self.calls == other.calls
            && self.context == other.context
            && self.step_base == other.step_base
            && self.tasks == other.tasks
            && self.runs == other.runs
            && self.children == other.children
            && self.compacting == other.compacting
            && self.parked == other.parked
            && self.compacted == other.compacted
            && self.unborn == other.unborn
    }
}

impl<P: Payload> Eq for Core<P> {}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TurnOpen => "a turn starts while another is running",
            Self::NoTurn => "no turn is running",
            Self::WrongTurn => "the record names a turn that is not running",
            Self::WrongStep => "the record names a step that is not current",
            Self::WrongStage => "the record does not fit the turn's stage",
            Self::Cancelling => "the turn is being cancelled",
            Self::UnknownCall => "the record names a call outside the current step",
            Self::WrongPhase => "the call is not in the phase the record needs",
            Self::Unresolved => "calls are still unresolved",
            Self::Epoch => "the epoch does not follow the current one",
            Self::WrongEnding => "the turn's ending does not match whether it was cancelled",
            Self::Overflow => "a counter would overflow",
            Self::DuplicateEnvelope => "the envelope was already delivered",
            Self::WrongGrant => "the grant names no rule in force that fits the call",
            Self::Reborn => "the actor is already born",
            Self::Unborn => "the actor is not born",
            Self::Stopped => "the actor is stopped",
            Self::Mismatch => "the envelope is not one this actor may be delivered",
            Self::NoMail => "a turn starts with an empty mailbox",
            Self::WrongCarry => "the step carries other than the batch at the head of the mailbox",
            Self::UnknownTask => "no running task has that id",
            Self::WrongStop => "the stop is not one this actor may send",
            Self::WrongResult => "the result claims more than v knows of the call",
            Self::WrongCut => "the compaction's cut is not its own record",
            Self::NotCompacting => "no compaction with that cut awaits its summary",
        })
    }
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "log record {} is not a valid next record: {}",
            self.seq, self.violation
        )
    }
}

impl core::error::Error for Corrupt {}
