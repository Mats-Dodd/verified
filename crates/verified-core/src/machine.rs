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
//! - approval policy: `Approved { by: Policy }` applies only to a `Safe`
//!   call; an `Unsafe` one (`edit`, `bash`) needs the user (`approver_fits`).
//! - one message, one turn: a `TurnStarted` for a message that already
//!   started one does not apply (recovery refuses such a log).
//! - `decide` and `resume` return records that apply, one after another,
//!   to the state they were given (never a refusal), and every `RunTool` they
//!   return is for a call that, once those records apply, is approved and
//!   dispatched by the very record its effect id names, in the current epoch.
//!   For `decide` that record is one of the decision's own (`decided`): a
//!   call is never started twice by later decisions. Only `resume` may rerun
//!   a call dispatched before it, and only a `Safe` one, in the new epoch
//!   (`resumed`).
//! - a turn the user asked to stop can only end `Cancelled` (`apply` refuses
//!   anything else), so `decide` never ends one otherwise.
//!
//! Model-checked, not proved (Stateright, `verified-model`, and the scenario
//! tests in `tests/turns.rs`):
//!
//! - the core has no concurrency rules: every approved call is dispatched
//!   at once, in proposal order, beside whatever already runs.
//! - after a crash, a dispatched `Unsafe` call without a result is resolved
//!   `Unknown`, never rerun or reported otherwise.

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

use core::fmt;

use vstd::prelude::*;

#[cfg(verus_only)]
use verified_domain::id::same_step;
#[cfg(verus_only)]
use verified_domain::record::rerun_of;
use verified_domain::{
    Approver, CallId, CallIx, Cause, EffectId, Epoch, Exit, Failure, Fault, Limit, Limits,
    Measured, MessageId, Millis, Payload, Proposal, Record, Request, Rerun, Reset, StepId, TurnId,
    TurnOutcome,
};

use crate::effect::{self, Decision, Effect, Outcome, Rejection};
#[cfg(verus_only)]
use crate::effect::{Approved, ToolCall};
use crate::event::{Event, ModelExit, Verdict};

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

/// One call as the spec sees it, with how many `Approved` and `Dispatched`
/// records were applied to it.
#[expect(missing_copy_implementations, reason = "spec-only type, never copied at run time")]
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `nat` values that exist only while verifying")]
pub struct CallView {
    pub phase: Phase,
    /// The rerun class of the call's tool; `None` for a request that did
    /// not parse into a known tool's arguments.
    pub rerun: Option<Rerun>,
    pub approvals: nat,
    pub dispatches: nat,
}

/// The abstract state the proofs talk about.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `Seq` values that exist only while verifying")]
pub struct CoreView {
    pub epoch: u64,
    pub seq: u64,
    /// Every message that started a turn, in order.
    pub messages: Seq<MessageId>,
    pub turn: Turn,
    pub calls: Seq<CallView>,
}

/// Approve before dispatch, at most once each, and only a valid request.
pub open spec fn call_ok(call: CallView) -> bool {
    &&& call.approvals <= 1
    &&& call.dispatches <= call.approvals
    &&& (call.phase is Proposed ==> call.approvals == 0)
    &&& (call.phase is Approved ==> call.approvals == 1 && call.dispatches == 0)
    &&& (call.phase is Dispatched ==> call.dispatches == 1)
    &&& (call.rerun is None ==> call.approvals == 0)
}

/// Whether `message` already started a turn.
pub open spec fn seen_message(messages: Seq<MessageId>, message: MessageId) -> bool {
    exists|at: int| 0 <= at < messages.len() && (#[trigger] messages[at])@ == message@
}

/// Policy approves only `Safe` calls; the user may approve any valid one.
pub open spec fn approver_fits(by: Approver, rerun: Option<Rerun>) -> bool {
    by is User || rerun == Some(Rerun::Safe)
}

pub open spec fn ledger_ok(calls: Seq<CallView>) -> bool {
    forall|index: int| 0 <= index < calls.len() ==> call_ok(#[trigger] calls[index])
}

/// Every call in `from..to` is resolved.
pub open spec fn resolved(calls: Seq<CallView>, from: int, to: int) -> bool {
    forall|index: int| from <= index < to ==> (#[trigger] calls[index]).phase is Resolved
}

/// The invariant of every state a log reaches: the ledger holds, and only
/// the current step's calls may be unresolved (none, outside `Tools`).
pub open spec fn state_ok(view: CoreView) -> bool {
    &&& ledger_ok(view.calls)
    &&& match view.turn {
        Turn::Idle => resolved(view.calls, 0, view.calls.len() as int),
        Turn::Open(open) => {
            &&& open.step.turn@ == open.turn@
            &&& open.first_call <= view.calls.len()
            &&& match open.stage {
                Stage::Tools => resolved(view.calls, 0, open.first_call as int),
                Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. } => resolved(
                    view.calls,
                    0,
                    view.calls.len() as int,
                ),
            }
        },
    }
}

pub open spec fn fresh_call(rerun: Option<Rerun>) -> CallView {
    CallView { phase: Phase::Proposed, rerun, approvals: 0, dispatches: 0 }
}

pub open spec fn initial() -> CoreView {
    CoreView { epoch: 0, seq: 0, messages: Seq::empty(), turn: Turn::Idle, calls: Seq::empty() }
}

/// The rerun class a proposed request asks for; `None` when it is invalid.
pub open spec fn request_rerun<P: Payload>(request: Request<P>) -> Option<Rerun> {
    match request {
        Request::Run { args } => Some(rerun_of(P::spec_tool(&args))),
        Request::Invalid { .. } => None,
    }
}

/// The calls a `StepFinished` adds.
pub open spec fn proposed<P: Payload>(calls: Seq<Proposal<P>>) -> Seq<CallView> {
    Seq::new(calls.len(), |index: int| fresh_call(request_rerun(calls[index].request)))
}

/// A turn ends `Cancelled` exactly when the user asked it to stop.
pub open spec fn ending_fits(intent: Intent, outcome: TurnOutcome) -> bool {
    (intent is Cancel) == (outcome is Cancelled)
}

/// What one record does to the abstract state; `None` when it cannot
/// follow it.
pub open spec fn spec_apply<P: Payload>(view: CoreView, record: Record<P>) -> Option<CoreView> {
    let seq = (view.seq + 1) as u64;
    match record {
        Record::TurnStarted { turn, message, .. } => match view.turn {
            Turn::Idle => if seen_message(view.messages, message) {
                None
            } else {
                Some(
                CoreView {
                    seq,
                    messages: view.messages.push(message),
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
                    ..view
                },
            )
            },
            Turn::Open(_) => None,
        },
        Record::StepStarted { step } => match view.turn {
            Turn::Idle => None,
            Turn::Open(open) => {
                if step.turn@ != open.turn@ || open.intent is Cancel {
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
                        Stage::Model { .. } => None,
                    };
                    match retries {
                        Some(retries) => Some(
                            CoreView {
                                seq,
                                turn: Turn::Open(
                                    Open { step, retries, stage: Stage::Model { since: view.seq }, ..open },
                                ),
                                ..view
                            },
                        ),
                        None => None,
                    }
                }
            },
        },
        Record::ModelOutput { step, .. } => match view.turn {
            Turn::Open(open) => if open.stage is Model && same_step(step, open.step) {
                Some(CoreView { seq, ..view })
            } else {
                None
            },
            Turn::Idle => None,
        },
        Record::StepReset { step, reason } => match view.turn {
            Turn::Open(open) => {
                if !(open.stage is Model) || open.intent is Cancel || !same_step(step, open.step) {
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
            Turn::Open(open) => if open.stage is Model && same_step(step, open.step) {
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
                if open.stage is Tools && open.intent is Proceed && open.first_call <= index
                    < view.calls.len() && view.calls[index].phase is Proposed
                    && view.calls[index].rerun is Some && approver_fits(by, view.calls[index].rerun) {
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
                if open.stage is Tools && open.intent is Proceed && open.first_call <= index
                    < view.calls.len() && view.calls[index].phase is Approved {
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
                            ..view
                        },
                    )
                } else {
                    None
                }
            },
            Turn::Idle => None,
        },
        Record::ToolResult { call, .. } => match view.turn {
            Turn::Open(open) => {
                let index = call.0 as int;
                if open.stage is Tools && open.first_call <= index < view.calls.len()
                    && !(view.calls[index].phase is Resolved) {
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
        Record::CancelRequested { turn } => match view.turn {
            Turn::Open(open) => if turn@ == open.turn@ && open.intent is Proceed {
                Some(
                    CoreView { seq, turn: Turn::Open(Open { intent: Intent::Cancel, ..open }), ..view },
                )
            } else {
                None
            },
            Turn::Idle => None,
        },
        Record::TurnEnded { turn, outcome } => match view.turn {
            Turn::Open(open) => if turn@ == open.turn@ && ending_fits(open.intent, outcome)
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

/// A rerun after recovery: the call was dispatched before the decision, it
/// is `Safe` (no side effect), and it runs in a new epoch, so its effect id
/// differs from the run it repeats. An `Unsafe` call never reruns.
pub open spec fn rerun<P: Payload>(start: CoreView, end: CoreView, call: ToolCall<Approved, P>) -> bool {
    let index = call.spec_call().0 as int;
    &&& call.spec_effect().seq < start.seq
    &&& end.epoch > start.epoch
    &&& end.calls[index].rerun == Some(Rerun::Safe)
}

/// Every `RunTool` in `effects` is licensed by `end` and dispatched by the
/// decision itself, or (only if `reruns`) a rerun after recovery.
pub open spec fn runs_ok<P: Payload>(
    start: CoreView,
    end: CoreView,
    effects: Seq<Effect<P>>,
    reruns: bool,
) -> bool {
    forall|index: int|
        0 <= index < effects.len() && (#[trigger] effects[index]) is RunTool ==> {
            let call = effects[index]->call;
            &&& licensed(end, call)
            &&& (fresh(start, call) || (reruns && rerun(start, end, call)))
        }
}

/// What `decide` promises: the records apply to `start` one after another,
/// and every tool it starts is licensed by the result and dispatched by one
/// of its own records. So a call runs at most once per `Dispatched`, and
/// nothing a decision did not dispatch is ever started by it.
pub open spec fn decided<P: Payload>(start: CoreView, decision: Decision<P>) -> bool {
    match applies(start, decision.records@) {
        Some(end) => state_ok(end) && runs_ok(start, end, decision.effects@, false),
        None => false,
    }
}

/// What `resume` promises: as `decided`, except that a call dispatched
/// before the crash may run again if it is `Safe`, in the new epoch.
pub open spec fn resumed<P: Payload>(start: CoreView, decision: Decision<P>) -> bool {
    match applies(start, decision.records@) {
        Some(end) => state_ok(end) && runs_ok(start, end, decision.effects@, true),
        None => false,
    }
}

/// Every call dispatched before `from` (and still unresolved) is `Safe`.
pub open spec fn rerunnable(view: CoreView, from: u64) -> bool {
    forall|index: int|
        0 <= index < view.calls.len() && dispatched_before((#[trigger] view.calls[index]).phase, from)
            ==> view.calls[index].rerun == Some(Rerun::Safe)
}

pub open spec fn dispatched_before(phase: Phase, from: u64) -> bool {
    match phase {
        Phase::Dispatched { seq } => seq < from,
        Phase::Proposed | Phase::Approved | Phase::Resolved => false,
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
/// it stays `Exceeded`: no wrap, no saturation, and every later step fails
/// with `Limit::Context` before it is sent.
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
}

struct Call<P: Payload> {
    id: CallId,
    /// `None` for a request that did not parse.
    args: Option<P::Args>,
    /// The rerun class of the call's tool, read from the arguments the
    /// record carried; `None` for an invalid request.
    rerun: Option<Rerun>,
    phase: Phase,
}

impl<P: Payload> Call<P> {
    #[expect(clippy::manual_map, reason = "Verus has no closures, so no `Option::map`")]
    fn copy(&self) -> (copy: Call<P>)
        ensures
            copy.phase == self.phase,
            copy.rerun == self.rerun,
            copy.args is Some == self.args is Some,
    {
        let args = match &self.args {
            Some(args) => Some(args.clone()),
            None => None,
        };
        Call { id: self.id.clone(), args, rerun: self.rerun, phase: self.phase }
    }

    /// The rerun class of the call's tool; `None` for an invalid request.
    const fn rerun(&self) -> (rerun: Option<Rerun>)
        ensures
            rerun == self.rerun,
    {
        self.rerun
    }
}

/// Whether the decision re-announces calls awaiting approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[verifier::external_derive]
enum Ask {
    Yes,
    No,
}

/// Which dispatched calls the decision starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[verifier::external_derive]
enum Runs {
    /// Those it dispatched itself.
    New,
    /// Every dispatched call without a result: after a crash, the ones
    /// still unresolved are reruns of `Safe` calls.
    All,
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
    /// A turn starts from a message that already started one.
    DuplicateMessage,
    /// Policy approved a call that only the user may approve.
    WrongApprover,
}

/// A record `apply` refused, and its position in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Corrupt {
    pub seq: u64,
    pub violation: Violation,
}

/// The agent's state for one session.
pub struct Core<P: Payload> {
    limits: Limits,
    epoch: Epoch,
    /// Records applied so far: the sequence number of the next one.
    seq: u64,
    /// Every message the session accepted, for deduplication.
    messages: Vec<MessageId>,
    turn: Turn,
    /// The calls of the running (or last) turn, indexed by `CallIx`.
    calls: Vec<Call<P>>,
    context: Context,
    /// Ghost audit of the calls: approvals and dispatches applied.
    ledger: Ghost<Seq<CallView>>,
}

impl<P: Payload> View for Core<P> {
    type V = CoreView;

    closed spec fn view(&self) -> CoreView {
        CoreView {
            epoch: self.epoch.0,
            seq: self.seq,
            messages: self.messages@,
            turn: self.turn,
            calls: self.ledger@,
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

/// The fault to retry on, if `cause` is retryable.
const fn retryable(cause: Cause) -> Option<Fault> {
    match cause {
        Cause::Failed(Fault::Overloaded) => Some(Fault::Overloaded),
        Cause::Failed(Fault::Network) => Some(Fault::Network),
        Cause::Failed(Fault::Other | Fault::Limit(_)) | Cause::Interrupted | Cause::Unknown
        | Cause::Denied => None,
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
    &&& call.rerun == view.rerun
    &&& (call.args is Some) == (call.rerun is Some)
}

/// Where a plan started: the state the decision applies to, and whether it
/// may rerun calls dispatched before it (only `resume`, once it has bumped
/// the epoch and resolved every dispatched call that may not run again).
#[cfg_attr(not(verus_keep_ghost), expect(dead_code, reason = "ghost state: read only by the proofs, erased in plain builds"))]
struct Origin {
    view: CoreView,
    reruns: bool,
}

/// A decision being drafted on a scratch copy of the core: the records so
/// far (each already applied to the copy) and the effects.
struct Plan<P: Payload> {
    start: Ghost<Origin>,
    /// The sequence number of the decision's first record.
    from: u64,
    scratch: Core<P>,
    records: Vec<Record<P>>,
    effects: Vec<Effect<P>>,
}

/// Applying a record keeps every call dispatched before `from` rerunnable,
/// and never lowers the epoch.
proof fn lemma_apply_keeps_reruns<P: Payload>(view: CoreView, record: Record<P>, from: u64)
    requires
        from <= view.seq,
        spec_apply(view, record) is Some,
    ensures
        rerunnable(view, from) ==> rerunnable(spec_apply(view, record)->0, from),
        spec_apply(view, record)->0.epoch >= view.epoch,
{
    let next = spec_apply(view, record)->0;
    if !rerunnable(view, from) {
        return;
    }
    assert forall|index: int|
        0 <= index < next.calls.len() && dispatched_before((#[trigger] next.calls[index]).phase, from)
        implies next.calls[index].rerun == Some(Rerun::Safe) by {
        match record {
            Record::StepFinished { calls, .. } => {
                if index >= view.calls.len() {
                    assert(next.calls[index] == proposed(calls@)[index - view.calls.len()]);
                } else {
                    assert(next.calls[index] == view.calls[index]);
                }
            },
            Record::Approved { call, .. } | Record::Dispatched { call } | Record::ToolResult {
                call,
                ..
            } => {
                if index != call.0 as int {
                    assert(next.calls[index] == view.calls[index]);
                }
            },
            Record::TurnStarted { .. } => {},
            Record::StepStarted { .. } | Record::ModelOutput { .. } | Record::StepReset { .. }
            | Record::CancelRequested { .. } | Record::TurnEnded { .. } | Record::EpochBumped {
                ..
            } => {
                assert(next.calls == view.calls);
            },
        }
    }
}

impl<P: Payload> Plan<P> {
    spec fn inv(&self) -> bool {
        &&& self.scratch.wf()
        &&& applies(self.start@.view, self.records@) == Some(self.scratch@)
        &&& self.from == self.start@.view.seq
        &&& self.from <= self.scratch@.seq
        &&& self.scratch@.epoch >= self.start@.view.epoch
        &&& self.start@.reruns ==> rerunnable(self.scratch@, self.from)
        &&& self.start@.reruns ==> self.scratch@.epoch > self.start@.view.epoch
        &&& forall|index: int|
            0 <= index < self.effects@.len() ==> !((#[trigger] self.effects@[index]) is RunTool)
    }

    /// Appends `record`, applying it to the scratch copy, and returns its
    /// sequence number.
    fn emit(&mut self, record: Record<P>) -> (seq: u64)
        requires
            old(self).inv(),
            spec_apply(old(self).scratch@, record) is Some,
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).from == old(self).from,
            final(self).scratch@ == spec_apply(old(self).scratch@, record)->0,
            final(self).effects@ == old(self).effects@,
            seq == old(self).scratch@.seq,
    {
        let seq = self.scratch.seq;
        proof {
            lemma_apply_keeps_reruns(self.scratch@, record, self.from);
        };
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

    /// Adds an effect that is not a tool run (those come from `finish`).
    fn effect(&mut self, effect: Effect<P>)
        requires
            old(self).inv(),
            !(effect is RunTool),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).from == old(self).from,
            final(self).scratch@ == old(self).scratch@,
    {
        self.effects.push(effect);
    }

    const fn id(&self, seq: u64) -> EffectId {
        EffectId { epoch: self.scratch.epoch, seq }
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
            !(old(self).scratch@.calls[index as int].phase is Resolved),
        ensures
            final(self).inv(),
            final(self).start == old(self).start,
            final(self).from == old(self).from,
            final(self).scratch@.turn == old(self).scratch@.turn,
            final(self).scratch@.epoch == old(self).scratch@.epoch,
            final(self).scratch@.calls.len() == old(self).scratch@.calls.len(),
            final(self).scratch@.calls[index as int].phase is Resolved,
            forall|other: int|
                0 <= other < final(self).scratch@.calls.len() && other != index ==> (
                #[trigger] final(self).scratch@.calls[other]) == old(self).scratch@.calls[other],
    {
        self.emit(Record::ToolResult { call: CallIx(index), exit, output: None });
    }

    /// Starts every tool run the decision licenses, announces calls
    /// awaiting the user if `ask`, and hands the decision over.
    #[verifier::loop_isolation(false)]
    #[expect(clippy::collapsible_if, reason = "Verus does not parse `if let` chains")]
    fn finish(self, outcome: Outcome, ask: Ask, runs: Runs) -> (decision: Decision<P>)
        requires
            self.inv(),
            runs is All ==> self.start@.reruns,
        ensures
            match runs {
                Runs::New => decided(self.start@.view, decision),
                Runs::All => resumed(self.start@.view, decision),
            },
    {
        let mut plan = self;
        let count = plan.scratch.calls.len();
        let ghost reruns = runs is All;
        let ghost origin = plan.start@;
        let mut index: usize = 0;
        while index < count
            invariant
                plan.scratch.wf(),
                plan.start@ == origin,
                applies(origin.view, plan.records@) == Some(plan.scratch@),
                plan.from == origin.view.seq,
                reruns ==> origin.reruns,
                origin.reruns ==> rerunnable(plan.scratch@, plan.from),
                origin.reruns ==> plan.scratch@.epoch > origin.view.epoch,
                count == plan.scratch.calls@.len(),
                runs_ok(origin.view, plan.scratch@, plan.effects@, reruns),
            decreases count - index,
        {
            if let Phase::Dispatched { seq } = plan.scratch.calls[index].phase {
                let start = match runs {
                    Runs::New => seq >= plan.from,
                    Runs::All => true,
                };
                if start {
                    if let Some(args) = &plan.scratch.calls[index].args {
                        let call = effect::approved(
                            CallIx(index),
                            plan.scratch.calls[index].id.clone(),
                            args.clone(),
                            EffectId { epoch: plan.scratch.epoch, seq },
                        );
                        proof {
                            assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
                            assert(call_ok(plan.scratch@.calls[index as int]));
                            if seq < plan.from {
                                assert(dispatched_before(plan.scratch@.calls[index as int].phase, plan.from));
                            }
                        };
                        let ghost before = plan.effects@;
                        plan.effects.push(Effect::RunTool { call });
                        proof {
                            assert forall|at: int|
                                0 <= at < plan.effects@.len() && (#[trigger] plan.effects@[at]) is RunTool
                                implies licensed(plan.scratch@, plan.effects@[at]->call) && (fresh(
                                origin.view,
                                plan.effects@[at]->call,
                            ) || (reruns && rerun(origin.view, plan.scratch@, plan.effects@[at]->call))) by {
                                if at < before.len() {
                                    assert(plan.effects@[at] == before[at]);
                                }
                            }
                        };
                    }
                }
            }
            index += 1;
        }
        match (ask, plan.scratch.turn) {
            (Ask::Yes, Turn::Open(open)) => match open.intent {
                Intent::Proceed => {
                    let mut index = open.first_call;
                    while index < count
                        invariant
                            plan.scratch.wf(),
                            count == plan.scratch.calls@.len(),
                            runs_ok(origin.view, plan.scratch@, plan.effects@, reruns),
                        decreases count - index,
                    {
                        if matches!(plan.scratch.calls[index].phase, Phase::Proposed) {
                            if let Some(args) = &plan.scratch.calls[index].args {
                                match P::tool(args).rerun() {
                                    Rerun::Unsafe => {
                                        let request = effect::pending(
                                            CallIx(index),
                                            plan.scratch.calls[index].id.clone(),
                                            args.clone(),
                                        );
                                        let ghost before = plan.effects@;
                                        plan.effects.push(Effect::AskApproval { request });
                                        proof {
                                            assert forall|at: int|
                                                0 <= at < plan.effects@.len() && (
                                                #[trigger] plan.effects@[at]) is RunTool implies licensed(
                                                plan.scratch@,
                                                plan.effects@[at]->call,
                                            ) && (fresh(origin.view, plan.effects@[at]->call) || (reruns
                                                && rerun(origin.view, plan.scratch@, plan.effects@[at]->call))) by {
                                                if at < before.len() {
                                                    assert(plan.effects@[at] == before[at]);
                                                }
                                            }
                                        };
                                    },
                                    Rerun::Safe => {},
                                }
                            }
                        }
                        index += 1;
                    }
                },
                Intent::Cancel => {},
            },
            (Ask::Yes | Ask::No, Turn::Idle) | (Ask::No, Turn::Open(_)) => {},
        }
        Decision { records: plan.records, effects: plan.effects, outcome }
    }
}

impl<P: Payload> Core<P> {
    /// The state ready to run under `limits`.
    pub closed spec fn wf(&self) -> bool {
        &&& self.ledger@.len() == self.calls@.len()
        &&& forall|index: int|
            0 <= index < self.calls@.len() ==> mirrors(#[trigger] self.calls@[index], self.ledger@[index])
        &&& state_ok(self@)
    }

    /// An empty session.
    #[must_use]
    pub fn new(limits: Limits) -> (core: Core<P>)
        ensures
            core.wf(),
            core@ == initial(),
    {
        let messages = Vec::with_capacity(limits.turns_per_session());
        let calls = Vec::with_capacity(limits.calls_per_turn());
        let core = Core {
            limits,
            epoch: Epoch(0),
            seq: 0,
            messages,
            turn: Turn::Idle,
            calls,
            context: Context::Within(0),
            ledger: Ghost(Seq::empty()),
        };
        assert(core@.calls =~= initial().calls);
        assert(core@.messages =~= initial().messages);
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

    /// The running turn, if any.
    #[must_use]
    pub const fn running(&self) -> Option<TurnId> {
        match self.turn {
            Turn::Open(open) => Some(open.turn),
            Turn::Idle => None,
        }
    }

    /// A scratch copy to plan on: same state, same view.
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
                        && calls@[at].rerun == self.calls@[at].rerun && (calls@[at].args is Some) == (
                    self.calls@[at].args is Some),
            decreases self.calls@.len() - index,
        {
            calls.push(self.calls[index].copy());
            index += 1;
        }
        let mut messages: Vec<MessageId> = Vec::with_capacity(self.messages.len());
        let mut at: usize = 0;
        while at < self.messages.len()
            invariant
                at <= self.messages@.len(),
                messages@ == self.messages@.take(at as int),
            decreases self.messages@.len() - at,
        {
            messages.push(self.messages[at]);
            at += 1;
            assert(messages@ =~= self.messages@.take(at as int));
        }
        assert(self.messages@.take(self.messages@.len() as int) =~= self.messages@);
        let copy = Core {
            limits: self.limits.clone(),
            epoch: self.epoch,
            seq: self.seq,
            messages,
            turn: self.turn,
            calls,
            context: self.context,
            ledger: Ghost(self.ledger@),
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

    /// Applies one durable record. The only way state changes; runs only
    /// after the log acknowledged `record`.
    ///
    /// # Errors
    ///
    /// `Corrupt` when `record` cannot follow the log so far; the core is
    /// left unchanged.
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
        match record {
            Record::TurnStarted { turn, message, text } => {
                if matches!(self.turn, Turn::Open(_)) {
                    return self.refuse(Violation::TurnOpen);
                }
                if self.seen(*message) {
                    return self.refuse(Violation::DuplicateMessage);
                }
                self.messages.push(*message);
                self.turn = Turn::Open(Open {
                    turn: *turn,
                    step: StepId { turn: *turn, index: 0 },
                    retries: 0,
                    stage: Stage::Starting,
                    intent: Intent::Proceed,
                    first_call: 0,
                });
                self.calls.clear();
                self.ledger = Ghost(Seq::empty());
                self.context = self.context.grow(text.bytes, self.limits.context_bytes());
                self.seq = next;
                assert(self@.calls =~= spec_apply(old(self)@, *record)->0.calls);
                Ok(())
            },
            Record::StepStarted { step } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
                if !step.turn.same(open.turn) {
                    return self.refuse(Violation::WrongTurn);
                }
                if matches!(open.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
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
                    Stage::Model { .. } => return self.refuse(Violation::WrongStage),
                };
                self.turn = Turn::Open(
                    Open { step: *step, retries, stage: Stage::Model { since: self.seq }, ..open },
                );
                self.seq = next;
                Ok(())
            },
            Record::ModelOutput { step, parts } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
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
            Record::StepReset { step, reason } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
                if !(matches!(open.stage, Stage::Model { .. })) {
                    return self.refuse(Violation::WrongStage);
                }
                if matches!(open.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
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
                self.turn = Turn::Open(
                    Open { retries, stage: Stage::Backoff { since: self.seq, reset: *reason }, ..open },
                );
                self.seq = next;
                Ok(())
            },
            Record::StepFinished { step, calls } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
                if !(matches!(open.stage, Stage::Model { .. })) {
                    return self.refuse(Violation::WrongStage);
                }
                if !step.same(open.step) {
                    return self.refuse(Violation::WrongStep);
                }
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
                    let (args, rerun) = match &proposal.request {
                        Request::Run { args } => (Some(args.clone()), Some(P::tool(args).rerun())),
                        Request::Invalid { .. } => (None, None),
                    };
                    proof {
                        assert(rerun == request_rerun(calls@[index as int].request));
                    };
                    self.calls.push(Call { id: proposal.id.clone(), args, rerun, phase: Phase::Proposed });
                    self.ledger = Ghost(self.ledger@.push(fresh_call(rerun)));
                    assert(calls@.take(index + 1).drop_last() == calls@.take(index as int));
                    assert(proposed(calls@.take(index + 1)) =~= proposed(calls@.take(index as int)).push(fresh_call(rerun)));
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
            },
            Record::Approved { call, by } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
                if !(matches!(open.stage, Stage::Tools)) {
                    return self.refuse(Violation::WrongStage);
                }
                if matches!(open.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
                }
                let index = call.0;
                if index < open.first_call || index >= self.calls.len() {
                    return self.refuse(Violation::UnknownCall);
                }
                assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
                if !(matches!(self.calls[index].phase, Phase::Proposed)) {
                    return self.refuse(Violation::WrongPhase);
                }
                if self.calls[index].rerun.is_none() {
                    return self.refuse(Violation::WrongPhase);
                }
                let fits = match by {
                    Approver::User => true,
                    Approver::Policy => matches!(self.calls[index].rerun, Some(Rerun::Safe)),
                };
                if !fits {
                    return self.refuse(Violation::WrongApprover);
                }
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
            },
            Record::Dispatched { call } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
                if !(matches!(open.stage, Stage::Tools)) {
                    return self.refuse(Violation::WrongStage);
                }
                if matches!(open.intent, Intent::Cancel) {
                    return self.refuse(Violation::Cancelling);
                }
                let index = call.0;
                if index < open.first_call || index >= self.calls.len() {
                    return self.refuse(Violation::UnknownCall);
                }
                assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
                if !(matches!(self.calls[index].phase, Phase::Approved)) {
                    return self.refuse(Violation::WrongPhase);
                }
                let ghost was = self.ledger@[index as int];
                self.calls[index].phase = Phase::Dispatched { seq: self.seq };
                self.ledger = Ghost(
                    self.ledger@.update(
                        index as int,
                        CallView {
                            phase: Phase::Dispatched { seq: self.seq },
                            dispatches: was.dispatches + 1,
                            ..was
                        },
                    ),
                );
                self.seq = next;
                assert(call_ok(self.ledger@[index as int]));
                Ok(())
            },
            Record::ToolResult { call, exit: _, output } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
                if !(matches!(open.stage, Stage::Tools)) {
                    return self.refuse(Violation::WrongStage);
                }
                let index = call.0;
                if index < open.first_call || index >= self.calls.len() {
                    return self.refuse(Violation::UnknownCall);
                }
                assert(mirrors(self.calls@[index as int], self.ledger@[index as int]));
                if matches!(self.calls[index].phase, Phase::Resolved) {
                    return self.refuse(Violation::WrongPhase);
                }
                let ghost was = self.ledger@[index as int];
                self.calls[index].phase = Phase::Resolved;
                self.ledger = Ghost(
                    self.ledger@.update(index as int, CallView { phase: Phase::Resolved, ..was }),
                );
                self.context = match output {
                    Some(output) => self.context.grow(output.bytes, self.limits.context_bytes()),
                    None => self.context,
                };
                self.seq = next;
                assert(call_ok(self.ledger@[index as int]));
                Ok(())
            },
            Record::CancelRequested { turn } => {
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
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
                let open = match self.turn {
                    Turn::Open(open) => open,
                    Turn::Idle => return self.refuse(Violation::NoTurn),
                };
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
        }
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
            plan.start@.view == self@,
            !plan.start@.reruns,
            plan.scratch@ == self@,
    {
        let plan = Plan {
            start: Ghost(Origin { view: self@, reruns: false }),
            from: self.seq,
            scratch: self.scratch(),
            records: Vec::new(),
            effects: Vec::new(),
        };
        assert(plan.records@.len() == 0);
        plan
    }

    /// Whether `message` already started a turn.
    fn seen(&self, message: MessageId) -> (seen: bool)
        ensures
            seen == seen_message(self@.messages, message),
    {
        let mut index: usize = 0;
        while index < self.messages.len()
            invariant
                index <= self.messages@.len(),
                forall|at: int| 0 <= at < index ==> (#[trigger] self.messages@[at])@ != message@,
            decreases self.messages@.len() - index,
        {
            if self.messages[index].same(message) {
                assert(self.messages@[index as int]@ == message@);
                return true;
            }
            index += 1;
        }
        false
    }

    /// Starts model call `step`, unless a bound says the turn is over.
    fn start_step(&self, plan: &mut Plan<P>, step: StepId)
        requires
            old(plan).inv(),
            may_start(old(plan).scratch@, step),
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).from == old(plan).from,
    {
        if step.index >= self.limits.steps_per_turn() {
            plan.emit(Record::TurnEnded { turn: step.turn, outcome: TurnOutcome::Failed(Failure::Limit(Limit::Steps)) });
        } else if matches!(plan.scratch.context, Context::Exceeded) {
            plan.emit(Record::TurnEnded { turn: step.turn, outcome: TurnOutcome::Failed(Failure::Limit(Limit::Context)) });
        } else {
            let seq = plan.emit(Record::StepStarted { step });
            let id = plan.id(seq);
            plan.effect(Effect::CallModel { id, step });
        }
    }

    /// Moves the current step's calls forward: interrupts, calls past a
    /// bound, invalid requests, policy approvals, dispatch of every approved
    /// call, and once every call is resolved, the next step or the end of
    /// the turn.
    #[verifier::loop_isolation(false)]
    fn settle(&self, plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.turn is Open,
            old(plan).scratch@.turn->Open_0.stage is Tools,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).from == old(plan).from,
    {
        let Some(open) = plan.open() else {
            return;
        };
        let count = plan.scratch.calls.len();
        // Interrupts, calls past a bound, invalid requests and policy.
        let mut index = open.first_call;
        while index < count
            invariant
                plan.inv(),
                plan.start == old(plan).start,
                plan.from == old(plan).from,
                plan.scratch@.turn == Turn::Open(open),
                plan.scratch@.calls.len() == count,
                open.first_call <= index,
            decreases count - index,
        {
            assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
            match plan.scratch.calls[index].phase {
                Phase::Proposed => match open.intent {
                    Intent::Cancel => plan.resolve(index, Exit::Err(Cause::Interrupted)),
                    Intent::Proceed => {
                        if index - open.first_call >= self.limits.calls_per_step() || index
                            >= self.limits.calls_per_turn() {
                            plan.resolve(index, Exit::Err(Cause::Failed(Fault::Limit(Limit::Calls))));
                        } else {
                            match plan.scratch.calls[index].rerun() {
                                None => plan.resolve(index, Exit::Err(Cause::Failed(Fault::Other))),
                                Some(Rerun::Safe) => {
                                    plan.emit(
                                        Record::Approved { call: CallIx(index), by: Approver::Policy },
                                    );
                                },
                                Some(Rerun::Unsafe) => {},
                            }
                        }
                    },
                },
                Phase::Approved => match open.intent {
                    Intent::Cancel => plan.resolve(index, Exit::Err(Cause::Interrupted)),
                    Intent::Proceed => {},
                },
                Phase::Dispatched { .. } | Phase::Resolved => {},
            }
            index += 1;
        }
        // Dispatch every approved call, in proposal order. The core has no
        // concurrency rules: what runs together is the tools' business.
        if matches!(open.intent, Intent::Proceed) {
            let mut index = open.first_call;
            while index < count
                invariant
                    plan.inv(),
                    plan.start == old(plan).start,
                    plan.from == old(plan).from,
                    plan.scratch@.turn == Turn::Open(open),
                    plan.scratch@.calls.len() == count,
                    open.first_call <= index,
                    open.intent is Proceed,
                decreases count - index,
            {
                assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
                if matches!(plan.scratch.calls[index].phase, Phase::Approved) {
                    plan.emit(Record::Dispatched { call: CallIx(index) });
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
                    if count == open.first_call {
                        plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Completed });
                    } else if open.step.index < u32::MAX {
                        self.start_step(plan, StepId { turn: open.turn, index: open.step.index + 1 });
                    } else {
                        plan.emit(
                            Record::TurnEnded {
                                turn: open.turn,
                                outcome: TurnOutcome::Failed(Failure::Limit(Limit::Steps)),
                            },
                        );
                    }
                },
            }
        }
    }

    /// Decides what an event means: records to persist and effects to start
    /// once they are durable. Mutates nothing. The records apply to this
    /// state one after another, and every tool run is licensed by them.
    pub fn decide(&self, event: Event<P>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
    {
        match event {
            Event::Message { message, turn, text } => self.on_message(message, turn, text),
            Event::Approve { call, verdict } => self.on_approve(&call, verdict),
            Event::Interrupt { turn } => self.on_interrupt(turn),
            Event::ModelParts { id, parts } => self.on_parts(id, parts),
            Event::ModelDone { id, result } => self.on_model_done(id, result),
            Event::ToolDone { id, exit, output } => self.on_tool_done(id, exit, output),
            Event::Woke { id } => self.on_woke(id),
        }
    }

    fn on_message(&self, message: MessageId, turn: TurnId, text: Measured<P::Text>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
    {
        if self.seen(message) {
            return quiet(Outcome::Exists);
        }
        if matches!(self.turn, Turn::Open(_)) {
            return quiet(Outcome::Rejected(Rejection::Busy));
        }
        if self.messages.len() >= self.limits.turns_per_session() {
            return quiet(Outcome::Rejected(Rejection::SessionFull));
        }
        let mut plan = self.plan();
        plan.emit(Record::TurnStarted { turn, message, text });
        self.start_step(&mut plan, StepId { turn, index: 0 });
        plan.finish(Outcome::Accepted, Ask::No, Runs::New)
    }

    fn on_approve(&self, call: &CallId, verdict: Verdict) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
    {
        let mut plan = self.plan();
        let Some(open) = plan.open() else {
            return quiet(Outcome::Rejected(Rejection::UnknownCall));
        };
        if !(matches!(open.stage, Stage::Tools)) {
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
        if !(matches!(plan.scratch.calls[index].phase, Phase::Proposed)) {
            return quiet(Outcome::Exists);
        }
        // A call still awaiting approval while the turn is being cancelled
        // cannot exist (the interrupt resolves it), and an invalid one never
        // awaits approval.
        if plan.scratch.calls[index].args.is_none() || matches!(open.intent, Intent::Cancel) {
            return quiet(Outcome::Rejected(Rejection::UnknownCall));
        }
        match verdict {
            Verdict::Approve => {
                plan.emit(Record::Approved { call: CallIx(index), by: Approver::User });
            },
            Verdict::Deny => plan.resolve(index, Exit::Err(Cause::Denied)),
        }
        self.settle(&mut plan);
        plan.finish(Outcome::Accepted, Ask::No, Runs::New)
    }

    fn on_interrupt(&self, turn: TurnId) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
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
        match open.stage {
            Stage::Starting | Stage::Backoff { .. } => {
                plan.emit(Record::TurnEnded { turn, outcome: TurnOutcome::Cancelled });
            },
            Stage::Model { since } => {
                let id = plan.id(since);
                plan.effect(Effect::CancelModel { id });
            },
            Stage::Tools => {
                let count = plan.scratch.calls.len();
                let mut index = open.first_call;
                while index < count
                    invariant
                        plan.inv(),
                        plan.start@.view == self@,
                        !plan.start@.reruns,
                        plan.scratch@.turn == Turn::Open(Open { intent: Intent::Cancel, ..open }),
                        plan.scratch@.calls.len() == count,
                    decreases count - index,
                {
                    if let Phase::Dispatched { seq } = plan.scratch.calls[index].phase {
                        let id = plan.id(seq);
                        plan.effect(Effect::CancelTool { id });
                    }
                    index += 1;
                }
                self.settle(&mut plan);
            },
        }
        plan.finish(Outcome::Accepted, Ask::No, Runs::New)
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
                Stage::Starting | Stage::Backoff { .. } | Stage::Tools => None,
            },
            Turn::Idle => None,
        }
    }

    fn on_parts(&self, id: EffectId, parts: Measured<P::Parts>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
    {
        let Some(open) = self.live_model(id) else {
            return quiet(Outcome::Stale);
        };
        let mut plan = self.plan();
        plan.emit(Record::ModelOutput { step: open.step, parts });
        plan.finish(Outcome::Accepted, Ask::No, Runs::New)
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
    {
        let Some(open) = self.live_model(id) else {
            return quiet(Outcome::Stale);
        };
        let mut plan = self.plan();
        let failed = match open.intent {
            Intent::Proceed => TurnOutcome::Failed(Failure::Model(Cause::Failed(Fault::Other))),
            Intent::Cancel => TurnOutcome::Cancelled,
        };
        match result {
            ModelExit::Finished(calls) => {
                if Self::reused(&calls) {
                    // Never two live calls with one id: the model boundary
                    // refuses such a response, and so does the core.
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: failed });
                } else {
                    plan.emit(Record::StepFinished { step: open.step, calls });
                    self.settle(&mut plan);
                }
            },
            ModelExit::Failed(cause) => match open.intent {
                Intent::Cancel => {
                    plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
                },
                Intent::Proceed => {
                    let retry = match retryable(cause) {
                        Some(fault) => match self.limits.retry_delay(open.retries) {
                            Some(delay) => Some(Reset::Retry { fault, delay }),
                            None => None,
                        },
                        None => None,
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
                                    outcome: TurnOutcome::Failed(Failure::Model(cause)),
                                },
                            );
                        },
                    }
                },
            },
        }
        plan.finish(Outcome::Accepted, Ask::Yes, Runs::New)
    }

    fn on_tool_done(&self, id: EffectId, exit: Exit, output: Measured<P::Output>) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
    {
        let mut plan = self.plan();
        let Some(open) = plan.open() else {
            return quiet(Outcome::Stale);
        };
        if !(matches!(open.stage, Stage::Tools)) || id.epoch.0 != self.epoch.0 {
            return quiet(Outcome::Stale);
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
            let named = match plan.scratch.calls[index].phase {
                Phase::Dispatched { seq } => seq == id.seq,
                Phase::Proposed | Phase::Approved | Phase::Resolved => false,
            };
            if named {
                found = Some(index);
            }
            index += 1;
        }
        let Some(index) = found else {
            return quiet(Outcome::Stale);
        };
        assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
        if !(matches!(plan.scratch.calls[index].phase, Phase::Dispatched { .. })) {
            return quiet(Outcome::Stale);
        }
        plan.emit(Record::ToolResult { call: CallIx(index), exit, output: Some(output) });
        self.settle(&mut plan);
        plan.finish(Outcome::Accepted, Ask::No, Runs::New)
    }

    fn on_woke(&self, id: EffectId) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            decided(self@, decision),
    {
        let mut plan = self.plan();
        let Some(open) = plan.open() else {
            return quiet(Outcome::Stale);
        };
        let since = match open.stage {
            Stage::Backoff { since, .. } => since,
            Stage::Starting | Stage::Model { .. } | Stage::Tools => return quiet(Outcome::Stale),
        };
        if !id.same(EffectId { epoch: self.epoch, seq: since }) {
            return quiet(Outcome::Stale);
        }
        match open.intent {
            Intent::Proceed => self.start_step(&mut plan, open.step),
            Intent::Cancel => {
                plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
            },
        }
        plan.finish(Outcome::Accepted, Ask::No, Runs::New)
    }

    // -----------------------------------------------------------------------
    // resume
    // -----------------------------------------------------------------------

    /// What to do after `recover`: bump the epoch so replies to effects from
    /// before the crash are stale, then pick up where the log stops. A
    /// dispatched call with no result is rerun if it is `Safe` and becomes
    /// `Unknown` if it is `Unsafe`; a model call in
    /// flight restarts its step; a backoff sleeps again. Like `decide`, the
    /// records apply and every tool run is licensed by them.
    pub fn resume(&self) -> (decision: Decision<P>)
        requires
            self.wf(),
        ensures
            resumed(self@, decision),
    {
        let mut plan = self.plan();
        let epoch = Epoch(bump(self.epoch.0));
        plan.emit(Record::EpochBumped { epoch });
        match plan.open() {
            Some(open) => {
                if !matches!(open.stage, Stage::Tools) {
                    // Nothing is dispatched outside `Tools`, so nothing can
                    // be rerun: reruns are allowed vacuously.
                    plan.start = Ghost(Origin { reruns: true, ..plan.start@ });
                }
                match (open.stage, open.intent) {
                    (Stage::Starting | Stage::Model { .. } | Stage::Backoff { .. }, Intent::Cancel) => {
                        plan.emit(Record::TurnEnded { turn: open.turn, outcome: TurnOutcome::Cancelled });
                    },
                    (Stage::Starting, Intent::Proceed) => {
                        self.start_step(&mut plan, StepId { turn: open.turn, index: 0 });
                    },
                    (Stage::Model { .. }, Intent::Proceed) => {
                        plan.emit(Record::StepReset { step: open.step, reason: Reset::Restart });
                        self.start_step(&mut plan, open.step);
                    },
                    (Stage::Backoff { since, reset }, Intent::Proceed) => {
                        let id = plan.id(since);
                        plan.effect(Effect::Sleep { id, delay: backoff(reset) });
                    },
                    (Stage::Tools, Intent::Proceed | Intent::Cancel) => {
                        Self::resume_tools(&mut plan);
                        // Every call still dispatched may run again, in the
                        // new epoch: from here on, reruns are allowed.
                        plan.start = Ghost(Origin { reruns: true, ..plan.start@ });
                        self.settle(&mut plan);
                    },
                }
            },
            None => {
                plan.start = Ghost(Origin { reruns: true, ..plan.start@ });
            },
        }
        plan.finish(Outcome::Accepted, Ask::Yes, Runs::All)
    }

    /// Resolves each dispatched call with no result that cannot be rerun:
    /// `Unsafe` calls become `Unknown`; when the turn is being cancelled,
    /// `Safe` ones become `Interrupted`.
    #[verifier::loop_isolation(false)]
    fn resume_tools(plan: &mut Plan<P>)
        requires
            old(plan).inv(),
            old(plan).scratch@.turn is Open,
            old(plan).scratch@.turn->Open_0.stage is Tools,
        ensures
            final(plan).inv(),
            final(plan).start == old(plan).start,
            final(plan).from == old(plan).from,
            final(plan).scratch@.turn == old(plan).scratch@.turn,
            final(plan).scratch@.epoch == old(plan).scratch@.epoch,
            forall|index: int|
                0 <= index < final(plan).scratch@.calls.len() && (
                #[trigger] final(plan).scratch@.calls[index]).phase is Dispatched
                    ==> final(plan).scratch@.calls[index].rerun == Some(Rerun::Safe),
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
                plan.from == old(plan).from,
                plan.scratch@.turn == Turn::Open(open),
                plan.scratch@.epoch == old(plan).scratch@.epoch,
                plan.scratch@.calls.len() == count,
                open.first_call <= index <= count,
                forall|at: int|
                    0 <= at < index && (#[trigger] plan.scratch@.calls[at]).phase is Dispatched
                        ==> plan.scratch@.calls[at].rerun == Some(Rerun::Safe),
            decreases count - index,
        {
            assert(mirrors(plan.scratch.calls@[index as int], plan.scratch.ledger@[index as int]));
            if let Phase::Dispatched { .. } = plan.scratch.calls[index].phase {
                match (plan.scratch.calls[index].rerun(), open.intent) {
                    (None | Some(Rerun::Unsafe), Intent::Proceed | Intent::Cancel) => {
                        plan.resolve(index, Exit::Err(Cause::Unknown));
                    },
                    (Some(Rerun::Safe), Intent::Cancel) => {
                        plan.resolve(index, Exit::Err(Cause::Interrupted));
                    },
                    (Some(Rerun::Safe), Intent::Proceed) => {},
                }
            }
            index += 1;
        }
    }
}

/// Whether model call `step` may start from `view`: the next `StepStarted`
/// applies, and so does a `TurnEnded` failing the turn by a bound.
pub open spec fn may_start(view: CoreView, step: StepId) -> bool {
    match view.turn {
        Turn::Open(open) => {
            &&& open.intent is Proceed
            &&& step.turn@ == open.turn@
            &&& resolved(view.calls, 0, view.calls.len() as int)
            &&& match open.stage {
                Stage::Starting => step.index == 0,
                Stage::Backoff { .. } => step.index == open.step.index,
                Stage::Tools => open.step.index < u32::MAX && step.index == open.step.index + 1,
                Stage::Model { .. } => false,
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
            .field("rerun", &self.rerun)
            .field("phase", &self.phase)
            .finish()
    }
}

impl<P: Payload> PartialEq for Call<P> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.args == other.args
            && self.rerun == other.rerun
            && self.phase == other.phase
    }
}

impl<P: Payload> fmt::Debug for Core<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("limits", &self.limits)
            .field("epoch", &self.epoch)
            .field("seq", &self.seq)
            .field("messages", &self.messages)
            .field("turn", &self.turn)
            .field("calls", &self.calls)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

/// Equal durable state. The ghost ledger is erased in plain builds.
impl<P: Payload> PartialEq for Core<P> {
    fn eq(&self, other: &Self) -> bool {
        self.limits == other.limits
            && self.epoch == other.epoch
            && self.seq == other.seq
            && self.messages == other.messages
            && self.turn == other.turn
            && self.calls == other.calls
            && self.context == other.context
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
            Self::DuplicateMessage => "the message already started a turn",
            Self::WrongApprover => "policy approved a call only the user may approve",
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
