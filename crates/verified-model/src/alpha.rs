//! α, the abstraction from any payload to [`Abstract`].
//!
//! A record keeps every id and every size, drops each body, and reduces a
//! call's arguments to the tool they name. The core reads nothing else of a
//! payload (it compares sizes against `Limits::context_bytes` and asks a
//! call's tool), so a core run on α of a log decides α of what it decides on
//! the log. An event likewise keeps its ids and sizes, and a call's arguments
//! only their tool.
//!
//! A `ToolCall` can only be minted inside the core, so an effect does not
//! become an `Effect<Abstract>`: it becomes [`Work`], the effect with every
//! field but the arguments, and a decision becomes [`Decided`]. The model
//! stages what it persists from a `Decided`, and the `alpha` test checks
//! that the core commutes with α: `decide` on α of a state and an event
//! decides α of what `decide` decides on them.

use verified_core::{Decision, Effect, Event, ModelExit, Outcome};
use verified_domain::{
    Abstract, CallId, CallIx, EffectId, Measured, Millis, Payload, Proposal, Record, Request,
    StepId, Tool, Unit,
};

/// A payload's size without its body.
#[must_use]
pub const fn measured<T>(measured: &Measured<T>) -> Measured<Unit> {
    Measured {
        body: Unit,
        bytes: measured.bytes,
    }
}

/// A proposal's id and the tool its arguments name; an invalid request
/// stays invalid.
#[must_use]
pub fn proposal<P: Payload>(proposal: &Proposal<P>) -> Proposal<Abstract> {
    Proposal {
        id: proposal.id.clone(),
        request: match &proposal.request {
            Request::Run { args } => Request::Run {
                args: P::tool(args),
            },
            Request::Invalid { .. } => Request::Invalid { reason: Unit },
        },
    }
}

/// α of a record.
#[must_use]
pub fn record<P: Payload>(record: &Record<P>) -> Record<Abstract> {
    match record {
        Record::TurnStarted {
            turn,
            message,
            text,
        } => Record::TurnStarted {
            turn: *turn,
            message: *message,
            text: measured(text),
        },
        Record::StepStarted { step } => Record::StepStarted { step: *step },
        Record::ModelOutput { step, parts } => Record::ModelOutput {
            step: *step,
            parts: measured(parts),
        },
        Record::StepReset { step, reason } => Record::StepReset {
            step: *step,
            reason: *reason,
        },
        Record::StepFinished { step, calls } => Record::StepFinished {
            step: *step,
            calls: calls.iter().map(proposal).collect(),
        },
        Record::Approved { call, by } => Record::Approved {
            call: *call,
            by: *by,
        },
        Record::Dispatched { call } => Record::Dispatched { call: *call },
        Record::ToolResult { call, exit, output } => Record::ToolResult {
            call: *call,
            exit: *exit,
            output: output.as_ref().map(measured),
        },
        Record::CancelRequested { turn } => Record::CancelRequested { turn: *turn },
        Record::TurnEnded { turn, outcome } => Record::TurnEnded {
            turn: *turn,
            outcome: *outcome,
        },
        Record::EpochBumped { epoch } => Record::EpochBumped { epoch: *epoch },
    }
}

/// α of an event.
#[must_use]
pub fn event<P: Payload>(event: &Event<P>) -> Event<Abstract> {
    match event {
        Event::Message {
            message,
            turn,
            text,
        } => Event::Message {
            message: *message,
            turn: *turn,
            text: measured(text),
        },
        Event::Approve { call, verdict } => Event::Approve {
            call: call.clone(),
            verdict: *verdict,
        },
        Event::Interrupt { turn } => Event::Interrupt { turn: *turn },
        Event::ModelParts { id, parts } => Event::ModelParts {
            id: *id,
            parts: measured(parts),
        },
        Event::ModelDone { id, result } => Event::ModelDone {
            id: *id,
            result: match result {
                ModelExit::Finished(calls) => {
                    ModelExit::Finished(calls.iter().map(proposal).collect())
                }
                ModelExit::Failed(cause) => ModelExit::Failed(*cause),
            },
        },
        Event::ToolDone { id, exit, output } => Event::ToolDone {
            id: *id,
            exit: *exit,
            output: measured(output),
        },
        Event::Woke { id } => Event::Woke { id: *id },
        Event::Expired { id } => Event::Expired { id: *id },
    }
}

/// An effect as the model sees it: every field but a call's arguments,
/// which keep only their tool.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Work {
    CallModel {
        id: EffectId,
        step: StepId,
    },
    CancelModel {
        id: EffectId,
    },
    AskApproval {
        call: CallIx,
        id: CallId,
        tool: Tool,
    },
    RunTool {
        call: CallIx,
        id: CallId,
        tool: Tool,
        effect: EffectId,
    },
    CancelTool {
        id: EffectId,
    },
    Sleep {
        id: EffectId,
        delay: Millis,
    },
}

/// α of an effect.
#[must_use]
pub fn work<P: Payload>(effect: &Effect<P>) -> Work {
    match effect {
        Effect::CallModel { id, step } => Work::CallModel {
            id: *id,
            step: *step,
        },
        Effect::CancelModel { id } => Work::CancelModel { id: *id },
        Effect::AskApproval { request } => Work::AskApproval {
            call: request.call(),
            id: request.id().clone(),
            tool: request.tool(),
        },
        Effect::RunTool { call } => Work::RunTool {
            call: call.call(),
            id: call.id().clone(),
            tool: call.tool(),
            effect: call.effect(),
        },
        Effect::CancelTool { id } => Work::CancelTool { id: *id },
        Effect::Sleep { id, delay } => Work::Sleep {
            id: *id,
            delay: *delay,
        },
    }
}

/// A decision as the model sees it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decided {
    pub records: Vec<Record<Abstract>>,
    pub effects: Vec<Work>,
    pub outcome: Outcome,
}

/// α of a decision.
#[must_use]
pub fn decision<P: Payload>(decision: &Decision<P>) -> Decided {
    Decided {
        records: decision.records.iter().map(record).collect(),
        effects: decision.effects.iter().map(work).collect(),
        outcome: decision.outcome,
    }
}
