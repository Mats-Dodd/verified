//! Durable records: the facts appended to the log before anything acts.
//!
//! An actor is `Born` once. Envelopes land in its mailbox as `Delivered`
//! records; a turn is `TurnStarted` (while the mailbox holds mail), then
//! steps (`StepStarted`, which carries the envelopes the model call
//! consumes, the model's raw output batched into `ModelOutput`s, then
//! either `StepReset` before a retry or `StepFinished` with the calls it
//! proposed), each call's `Approved`, `Dispatched` and `ToolResult`, and
//! finally exactly one `TurnEnded`. A background call's `Dispatched` starts
//! a task, as a `StopSent` does; `TaskEnded` ends it and is its
//! notification. Records are never changed once written. JSON shape: one
//! object per line, internally tagged by `"type"`.

use vstd::prelude::*;

use crate::envelope::{Envelope, EnvelopeId};
use crate::id::{ActorId, CallId, CallIx, Epoch, StepId, TaskId, TurnId};
use crate::limits::Millis;
use crate::payload::{Measured, Payload};
use crate::rules::{Born, Grant, Rules};

verus! {

/// Every kind of effect a call may ask for.
///
/// Every kind passes the one gate: a grant, then a durable `Dispatched`
/// (A1). The shell offers the model the tools its `Born` names.
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
    /// The call was stopped (an interrupt, or a stop of its actor).
    Cancelled,
}

/// Why an effect provably never acted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Unstarted {
    /// The user refused it.
    Denied,
    /// It awaited approval when a user message arrived: the user steered.
    Steered,
    /// Cancelled before it acted: the turn was interrupted before dispatch,
    /// or its run saw the cancel before any side effect.
    Cancelled,
    /// Its actor was stopped before it acted.
    Stopped,
    /// Past a bound: calls per step or turn, tasks, spawn depth, or the
    /// recipient's messages.
    Limit,
    /// The request did not parse into a known tool's arguments.
    Invalid,
    /// The recipient is unknown, unborn or stopped.
    Refused,
}

/// How an effect ended, claiming no more than v knows (H2).
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
    /// The rules in force from here on (A2), and the text of the rules
    /// file they were read from (`None`: there is none, so the default
    /// rules). Recorded whenever they are loaded or change.
    RulesLoaded { rules: Rules, file: Option<Measured<P::Text>> },
    /// An envelope landed in this actor's mailbox. At most once per id.
    Delivered { envelope: Envelope<P> },
    /// A turn starts, while the mailbox holds mail. Its id is this
    /// record's seq.
    TurnStarted { turn: TurnId },
    /// A model call begins, carrying `carries`: the envelopes at the head
    /// of the mailbox it consumes (a user message alone, or the run of
    /// other envelopes before the next one). A second `StepStarted` for the
    /// same step is a retry after a `StepReset` and carries nothing.
    StepStarted { step: StepId, carries: Vec<EnvelopeId> },
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
    /// What a call actually did. At most one per call (H1); one for every
    /// call once the turn is quiescent is held, not promised. `output` is
    /// absent when the core synthesized the result: `NotStarted` for a call
    /// never dispatched (denied, cancelled, invalid, past a limit), and
    /// `Unknown` for a dispatched one (a crash, a deadline).
    ToolResult { call: CallIx, exit: Exit, output: Option<Measured<P::Output>> },
    /// Task `task` ended, at most once; this record is also its
    /// notification, an envelope from `v` in this actor's own mailbox.
    /// `output` is absent when the core synthesized the end: `Unknown`
    /// (a crash or a deadline), or `NotStarted` for a `Safe` task.
    TaskEnded { task: TaskId, exit: Exit, output: Option<Measured<P::Output>> },
    /// This stopped actor stops `child`, one it spawned: a `Keyed` task
    /// that delivers the stop.
    StopSent { child: ActorId },
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
            | Record::Delivered { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
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

#[expect(
    clippy::multiple_inherent_impl,
    reason = "plain Rust beside the impl inside `verus!`: no proof mentions it"
)]
impl Tool {
    /// Every tool, in declaration order.
    pub const ALL: [Self; 8] = [
        Self::Read,
        Self::Edit,
        Self::Bash,
        Self::Spawn,
        Self::Send,
        Self::Wake,
        Self::Tasks,
        Self::Cancel,
    ];
}
