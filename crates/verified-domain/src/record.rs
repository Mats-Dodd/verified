//! Durable records: the facts appended to the log before anything acts.
//!
//! A turn is `TurnStarted`, then steps (`StepStarted`, the model's raw
//! output batched into `ModelOutput`s, then either `StepReset` before a
//! retry or `StepFinished` with the calls it proposed), each call's
//! `Approved`, `Dispatched` and `ToolResult`, and finally exactly one
//! `TurnEnded`. Records are never changed once written. JSON shape: one
//! object per line, internally tagged by `"type"`.

use vstd::prelude::*;

use crate::id::{CallId, CallIx, Epoch, MessageId, StepId, TurnId};
use crate::limits::Millis;
use crate::payload::{Measured, Payload};

verus! {

/// The tools v0 offers the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Tool {
    Read,
    Edit,
    Bash,
}

/// Whether a call may run again after a crash. v controls dispatch, not the
/// world: a call is either free of side effects or it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Rerun {
    /// No side effect: run it again.
    Safe,
    /// It may act on the world: a dispatched call without a result becomes
    /// `Unknown`, and only the user approves it.
    Unsafe,
}

/// The rerun class of `tool`, as the proofs see it. A new tool is `Unsafe`
/// unless it is known to have no side effect.
pub open spec fn rerun_of(tool: Tool) -> Rerun {
    match tool {
        Tool::Read => Rerun::Safe,
        Tool::Edit | Tool::Bash => Rerun::Unsafe,
    }
}

impl Tool {
    #[must_use]
    pub const fn rerun(self) -> (rerun: Rerun)
        ensures
            rerun == rerun_of(self),
    {
        match self {
            Tool::Read => Rerun::Safe,
            Tool::Edit | Tool::Bash => Rerun::Unsafe,
        }
    }
}

/// What a proposed call asks for, as the shell parsed it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(bound = "P: crate::payload::Codec")]
pub enum Request<P: Payload> {
    /// A known tool with arguments that parsed; the arguments name the tool.
    Run { args: P::Args },
    /// An unknown tool or arguments that did not parse; the call fails
    /// without running and the model is told why.
    Invalid { reason: P::Reason },
}

/// One tool call the model proposed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(bound = "P: crate::payload::Codec")]
pub struct Proposal<P: Payload> {
    pub id: CallId,
    pub request: Request<P>,
}

/// Why an attempt did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Fault {
    /// The model provider is overloaded: retried on schedule.
    Overloaded,
    /// The connection failed: retried on schedule.
    Network,
    /// Anything else: not retried.
    Other,
    /// A call past `calls_per_step` or `calls_per_turn`: it fails without
    /// running.
    TooManyCalls,
    /// The model's response (or a batch of it) is longer than the log's line
    /// bound: it is too large to record.
    TooLarge,
}

/// Why something did not run to success.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Cause {
    Failed(Fault),
    /// Stopped by an interrupt, or never started because of one.
    Interrupted,
    /// It may or may not have happened: a crash came between dispatch and
    /// result for a call that cannot be safely rerun.
    Unknown,
    /// The user refused the call.
    Denied,
}

/// How an effect ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Exit {
    Ok,
    Err(Cause),
}

/// Who approved a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Approver {
    /// The user, through an approve command.
    User,
    /// Policy: `Rerun::Safe` tools are approved without asking.
    Policy,
}

/// Why a step's output so far is discarded (an AI SDK `reset-step`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Reset {
    /// The call failed with a retryable fault; it is retried after `delay`.
    Retry { fault: Fault, delay: Millis },
    /// The daemon restarted while the call was in flight.
    Restart,
}

/// A bound from `Limits` that ended a turn before its next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Limit {
    /// The turn used `steps_per_turn`.
    Steps,
    /// The conversation outgrew `context_bytes`.
    Context,
}

/// Why a turn failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Failure {
    Limit(Limit),
    Model(Cause),
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed(Failure),
}

/// A fact in the log.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(tag = "type", bound = "P: crate::payload::Codec")]
pub enum Record<P: Payload> {
    /// The user's message starts a turn.
    TurnStarted { turn: TurnId, message: MessageId, text: Measured<P::Text> },
    /// A model call begins. A second `StepStarted` for the same step is a
    /// retry after a `StepReset`.
    StepStarted { step: StepId },
    /// A batch of what the model streamed, exactly as delivered.
    ModelOutput { step: StepId, parts: Measured<P::Parts> },
    /// The output of the current attempt is discarded before a retry.
    StepReset { step: StepId, reason: Reset },
    /// The model finished the step, proposing these calls (none ends the
    /// turn). Every call the model made is here, including any past a bound:
    /// those are resolved at once as failed by the limit.
    StepFinished { step: StepId, calls: Vec<Proposal<P>> },
    /// A call may run. Always precedes its `Dispatched`.
    Approved { call: CallIx, by: Approver },
    /// A call is about to run. At most one per call.
    Dispatched { call: CallIx },
    /// What a call actually did. At most one per call (U8); one for every
    /// call once the turn is quiescent is held, not promised. `output` is
    /// absent when the core synthesized the result (denied, interrupted
    /// before it ran, unknown after a crash, invalid request, past a limit).
    ToolResult { call: CallIx, exit: Exit, output: Option<Measured<P::Output>> },
    /// The user asked to stop the turn.
    CancelRequested { turn: TurnId },
    /// The turn is over. At most one per `TurnStarted`. A turn the user
    /// asked to stop ends `Cancelled`, whatever else arrived after.
    TurnEnded { turn: TurnId, outcome: TurnOutcome },
    /// The daemon recovered; effects from earlier epochs are stale.
    EpochBumped { epoch: Epoch },
}

} // verus!

/// The call table: each record of `log` with the call it names, if any.
///
/// An `Approved`, `Dispatched` or `ToolResult` names the call its `CallIx`
/// points at among those its turn proposed; a call is its number among the
/// log's proposals, in order, and its proposal. Every other record, and one
/// pointing past its turn's calls, names none.
pub fn named<'log, P, L>(
    log: L,
) -> impl Iterator<Item = (&'log Record<P>, Option<(usize, &'log Proposal<P>)>)>
where
    P: Payload + 'log,
    L: IntoIterator<Item = &'log Record<P>>,
{
    let mut proposals: Vec<&'log Proposal<P>> = Vec::new();
    let mut turn = 0usize;
    log.into_iter().map(move |record| {
        let call = match record {
            Record::TurnStarted { .. } => {
                turn = proposals.len();
                None
            }
            Record::StepFinished { calls, .. } => {
                proposals.extend(calls);
                None
            }
            Record::Approved { call, .. }
            | Record::Dispatched { call }
            | Record::ToolResult { call, .. } => turn
                .checked_add(call.0)
                .and_then(|number| Some((number, *proposals.get(number)?))),
            Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        };
        (record, call)
    })
}
