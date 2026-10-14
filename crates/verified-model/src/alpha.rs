//! α, the abstraction from any payload to [`Abstract`].
//!
//! A record keeps every id and every size, drops each body, and reduces a
//! call's arguments to their shape without its subject (a path, a command):
//! a rule's pattern is matched in the model against an empty subject. The matcher itself is proved (A2); the model checks grants up
//! to subjects, and commutes with the core under rules whose patterns are
//! `Any` (the default rules). The core reads nothing else of a
//! payload (it compares sizes against `Limits::context_bytes` and asks a
//! call's shape), so a core run on α of a log decides α of what it decides
//! on the log. An event likewise keeps its ids and sizes, and a call's
//! arguments only their shape without its subject.
//!
//! A `ToolCall` can only be minted inside the core, so an effect does not
//! become an `Effect<Abstract>`: it becomes [`Work`], the effect with every
//! field but the arguments, and a decision becomes [`Decided`]. The model
//! stages what it persists from a `Decided`, and the `alpha` test checks
//! that the core commutes with α: `decide` on α of a state and an event
//! decides α of what `decide` decides on them.

use verified_core::{Decision, Effect, Event, ModelExit, Outcome};
use verified_domain::{
    Abstract, ActorId, Body, CallId, CallIx, EffectId, Envelope, Measured, Millis, Payload,
    Proposal, Record, Request, Shape, StepId, Tool, Unit,
};

/// A payload's size without its body.
#[must_use]
pub const fn measured<T>(measured: &Measured<T>) -> Measured<Unit> {
    Measured {
        body: Unit,
        bytes: measured.bytes,
    }
}

/// A proposal's id and its arguments' shape without its subject; an
/// invalid request stays invalid.
#[must_use]
pub fn proposal<P: Payload>(proposal: &Proposal<P>) -> Proposal<Abstract> {
    Proposal {
        id: proposal.id.clone(),
        request: match &proposal.request {
            Request::Run { args } => Request::Run {
                args: P::shape(args).without_subject(),
            },
            Request::Invalid { .. } => Request::Invalid { reason: Unit },
        },
    }
}

/// α of an envelope: its ids, and its text's size.
#[must_use]
pub const fn envelope<P: Payload>(envelope: &Envelope<P>) -> Envelope<Abstract> {
    Envelope {
        id: envelope.id,
        from: envelope.from,
        to: envelope.to,
        reply_to: envelope.reply_to,
        body: match &envelope.body {
            Body::Text(text) => Body::Text(measured(text)),
            Body::Stop => Body::Stop,
        },
    }
}

/// α of a record.
#[must_use]
pub fn record<P: Payload>(record: &Record<P>) -> Record<Abstract> {
    match record {
        Record::Delivered {
            envelope: delivered,
        } => Record::Delivered {
            envelope: envelope(delivered),
        },
        Record::TurnStarted { turn } => Record::TurnStarted { turn: *turn },
        Record::Born { born } => Record::Born { born: born.clone() },
        Record::RulesLoaded { rules, file } => Record::RulesLoaded {
            rules: rules.clone(),
            file: file.as_ref().map(measured),
        },
        Record::StepStarted { step, carries } => Record::StepStarted {
            step: *step,
            carries: carries.clone(),
        },
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
        Record::TaskEnded { task, exit, output } => Record::TaskEnded {
            task: *task,
            exit: *exit,
            output: output.as_ref().map(measured),
        },
        Record::StopSent { child } => Record::StopSent { child: *child },
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
        Event::Born { born } => Event::Born { born: born.clone() },
        Event::Rules { rules, file } => Event::Rules {
            rules: rules.clone(),
            file: file.as_ref().map(measured),
        },
        Event::Deliver {
            envelope: delivered,
        } => Event::Deliver {
            envelope: envelope(delivered),
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
/// which keep only their tool and their shape without its subject.
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
    /// A run: its call, and the call's shape without its subject (which
    /// says whether it is a task, and a spawn's child).
    RunTool {
        call: CallIx,
        id: CallId,
        tool: Tool,
        shape: Shape,
        effect: EffectId,
    },
    CancelTool {
        id: EffectId,
    },
    Sleep {
        id: EffectId,
        delay: Millis,
    },
    Stop {
        id: EffectId,
        child: ActorId,
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
            shape: P::shape(call.args()).without_subject(),
            effect: call.effect(),
        },
        Effect::CancelTool { id } => Work::CancelTool { id: *id },
        Effect::Sleep { id, delay } => Work::Sleep {
            id: *id,
            delay: *delay,
        },
        Effect::Stop { id, child } => Work::Stop {
            id: *id,
            child: *child,
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
