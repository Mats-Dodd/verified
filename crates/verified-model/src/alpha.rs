//! α for the core's own types, on top of [`verified_domain::alpha`] for
//! records: an event keeps its ids and sizes, and a call's arguments only
//! their tool.
//!
//! A `ToolCall` can only be minted inside the core, so an effect does not
//! become an `Effect<Abstract>`: it becomes [`Work`], the effect with every
//! field but the arguments, and a decision becomes [`Decided`]. The model
//! stages what it persists from a `Decided`, and the `alpha` test checks
//! that the core commutes with α: `decide` on α of a state and an event
//! decides α of what `decide` decides on them.

use verified_core::{Decision, Effect, Event, ModelExit, Outcome};
use verified_domain::alpha::{measured, proposal, record};
use verified_domain::{Abstract, CallId, CallIx, EffectId, Millis, Payload, Record, StepId, Tool};

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
