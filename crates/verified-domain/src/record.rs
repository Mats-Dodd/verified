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
use crate::rules::{Born, Grant, Rules};

verus! {

/// Every kind of effect a call may ask for.
///
/// The shell offers the model `read`, `edit` and `bash`; the others
/// (actors and tasks) are named here so that rules and grants already cover
/// every kind (A4), and are offered once the runtime runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Tool {
    Read,
    Edit,
    Bash,
    Spawn,
    Send,
    Wake,
    Tasks,
    Cancel,
}

/// What a crash does to an effect. v controls dispatch, not the world: an
/// effect is free of side effects, or may change the world, or its target
/// deduplicates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Class {
    /// Repeating it is harmless: started again until it resolves.
    Safe,
    /// It may change the world: never started again; a dispatched call
    /// without a result becomes `Unknown`.
    Unsafe,
    /// Its target deduplicates it by an id v minted before sending: started
    /// again with the same id, and it takes effect once.
    Keyed,
}

/// The class of `tool`, as the proofs see it. A new tool is `Unsafe`
/// unless it is known to have no side effect.
pub open spec fn class_of(tool: Tool) -> Class {
    match tool {
        Tool::Read | Tool::Wake | Tool::Tasks | Tool::Cancel => Class::Safe,
        Tool::Edit | Tool::Bash => Class::Unsafe,
        Tool::Spawn | Tool::Send => Class::Keyed,
    }
}

impl Tool {
    /// Whether two tools are the same.
    #[must_use]
    pub const fn same(self, other: Tool) -> (same: bool)
        ensures
            same == (self == other),
    {
        matches!(
            (self, other),
            (Tool::Read, Tool::Read)
                | (Tool::Edit, Tool::Edit)
                | (Tool::Bash, Tool::Bash)
                | (Tool::Spawn, Tool::Spawn)
                | (Tool::Send, Tool::Send)
                | (Tool::Wake, Tool::Wake)
                | (Tool::Tasks, Tool::Tasks)
                | (Tool::Cancel, Tool::Cancel)
        )
    }

    #[must_use]
    pub const fn class(self) -> (class: Class)
        ensures
            class == class_of(self),
    {
        match self {
            Tool::Read | Tool::Wake | Tool::Tasks | Tool::Cancel => Class::Safe,
            Tool::Edit | Tool::Bash => Class::Unsafe,
            Tool::Spawn | Tool::Send => Class::Keyed,
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

/// Why a model call failed. A model call is not an effect (PRINCIPLES):
/// this is not an [`Exit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum ModelFault {
    /// The model provider is overloaded: retried on schedule.
    Overloaded,
    /// The connection failed: retried on schedule.
    Network,
    /// Anything else: not retried.
    Other,
    /// The model's response (or a batch of it) is longer than the log's line
    /// bound: it is too large to record.
    TooLarge,
    /// The call was stopped (an interrupt, or a full log ending the turn).
    Cancelled,
}

/// Why an effect provably never acted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Unstarted {
    /// The user refused it.
    Denied,
    /// Cancelled before it acted: the turn was interrupted before dispatch,
    /// or its run saw the cancel before any side effect.
    Cancelled,
    /// Past `calls_per_step` or `calls_per_turn`.
    Limit,
    /// The request did not parse into a known tool's arguments.
    Invalid,
}

/// How an effect ended, claiming no more than v knows (H4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Exit {
    /// It ran to completion and reported success.
    Ok,
    /// It ran to completion and reported failure.
    Failed,
    /// It provably never acted.
    NotStarted(Unstarted),
    /// It may have acted: a crash, a cancel or a deadline came while it ran.
    Unknown,
}

/// Why a step's output so far is discarded (an AI SDK `reset-step`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Reset {
    /// The call failed with a retryable fault; it is retried after `delay`.
    Retry { fault: ModelFault, delay: Millis },
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
    Model(ModelFault),
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
    /// Who this actor is. At most once.
    Born { born: Born },
    /// The rules in force from here on (A3), and the text of the rules
    /// file they were read from (`None`: there is none, so the default
    /// rules). Recorded whenever they are loaded or change.
    RulesLoaded { rules: Rules, file: Option<Measured<P::Text>> },
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
    /// A call may run, under `by`: the user's command or a rule in force
    /// that fits it. Always precedes its `Dispatched`.
    Approved { call: CallIx, by: Grant },
    /// A call is about to run. At most one per call.
    Dispatched { call: CallIx },
    /// What a call actually did. At most one per call (H2); one for every
    /// call once the turn is quiescent is held, not promised. `output` is
    /// absent when the core synthesized the result: `NotStarted` for a call
    /// never dispatched (denied, cancelled, invalid, past a limit), and
    /// `Unknown` for a dispatched one (a crash, a deadline).
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
            Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        };
        (record, call)
    })
}
