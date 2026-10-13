//! Projects committed records into AI SDK UI chunks: what clients see.
//!
//! A port of q's `StreamProjection.ts`, extended with tool chunks, approvals
//! and `reset-step`. Deterministic: chunks depend only on the records (and
//! the model name), so the same log always projects to the same stream.
//! Stream offsets are record seqs; a record may project to no chunks.

use serde_json::json;
use verified_domain::id::{ApprovalId, CallId, PartId, TurnId};
use verified_domain::{
    Approver, Cause, Exit, Failure, Fault, Limit, Record, Request, Rerun, TurnOutcome,
};
use verified_wire::chunk::{
    FinishReason, TurnEndReason, TurnEndedData, TurnStartedData, UiChunk, UserMessageData,
};

use crate::history::synthesized_text;
use crate::payload::{ModelPart, Parsed};

/// Where the current step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Closed,
    Open,
    /// The attempt was discarded; the retry continues the same UI step.
    Reset,
}

#[derive(Debug, Clone)]
struct Call {
    id: CallId,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Turn {
    step: StepState,
    step_index: u32,
    calls: Vec<Call>,
    cancelling: bool,
    finish: FinishReason,
}

/// The projection's state: what earlier records said that later ones need.
#[derive(Debug, Clone)]
pub struct Projector {
    model: String,
    turn: Option<Turn>,
}

fn part_id(step: u32, block: u32) -> PartId {
    PartId::parse(&format!("t{step}-{block}")).expect("digits, `t` and `-` make a part id")
}

fn approval_id(call: &CallId) -> ApprovalId {
    ApprovalId::from(call)
}

fn failure_text(failure: Failure) -> String {
    match failure {
        Failure::Limit(Limit::Steps) => "The turn used every model step it may take.".to_owned(),
        Failure::Model(Cause::Failed(Fault::TooLarge)) => {
            "The model's response is too large to record.".to_owned()
        }
        Failure::Model(Cause::Failed(Fault::TooManyCalls)) => {
            "The model proposed more tool calls than allowed.".to_owned()
        }
        Failure::Limit(Limit::Context) => {
            "The conversation is larger than the context limit.".to_owned()
        }
        Failure::Model(Cause::Failed(Fault::Overloaded)) => {
            "The model is overloaded; retries ran out.".to_owned()
        }
        Failure::Model(Cause::Failed(Fault::Network)) => {
            "The network failed; retries ran out.".to_owned()
        }
        Failure::Model(Cause::Failed(Fault::Other)) => "The model call failed.".to_owned(),
        Failure::Model(Cause::Interrupted) => "The model call was interrupted.".to_owned(),
        Failure::Model(Cause::Unknown) => "The model call's outcome is unknown.".to_owned(),
        Failure::Model(Cause::Denied) => "The model call was refused.".to_owned(),
    }
}

impl Projector {
    /// A projector for a daemon talking to `model`.
    #[must_use]
    pub const fn new(model: String) -> Self {
        Self { model, turn: None }
    }

    /// The chunks one record becomes, in order.
    #[must_use]
    pub fn project(&mut self, record: &Record<Parsed>) -> Vec<UiChunk> {
        let mut chunks = Vec::new();
        match record {
            Record::TurnStarted {
                turn,
                message,
                text,
            } => {
                let id = *turn;
                self.turn = Some(Turn {
                    step: StepState::Closed,
                    step_index: 0,
                    calls: Vec::new(),
                    cancelling: false,
                    finish: FinishReason::Stop,
                });
                chunks.push(UiChunk::UserMessage {
                    id,
                    payload: UserMessageData {
                        text: text.body.clone(),
                        message_id: Some(*message),
                    },
                });
                chunks.push(UiChunk::TurnStarted {
                    id,
                    payload: TurnStartedData {
                        model: self.model.clone(),
                    },
                });
                chunks.push(UiChunk::Start {
                    message_id: Some(id.to_string()),
                });
            }
            Record::StepStarted { step } => {
                if let Some(turn) = &mut self.turn {
                    turn.step_index = step.index;
                    match turn.step {
                        StepState::Reset => {}
                        StepState::Open => {
                            chunks.push(UiChunk::FinishStep);
                            chunks.push(UiChunk::StartStep);
                        }
                        StepState::Closed => chunks.push(UiChunk::StartStep),
                    }
                    turn.step = StepState::Open;
                }
            }
            Record::ModelOutput { step, parts } => {
                if let Some(turn) = &mut self.turn {
                    for part in &parts.body {
                        part_chunks(step.index, part, turn, &mut chunks);
                    }
                }
            }
            Record::StepReset { .. } => {
                if let Some(turn) = &mut self.turn {
                    turn.step = StepState::Reset;
                    chunks.push(UiChunk::ResetStep);
                }
            }
            Record::StepFinished { calls, .. } => {
                if let Some(turn) = &mut self.turn {
                    for proposal in calls {
                        let id = proposal.id.clone();
                        let (reason, asks) = match &proposal.request {
                            Request::Run { args } => {
                                (None, matches!(args.tool().rerun(), Rerun::Unsafe))
                            }
                            Request::Invalid { reason } => (Some(reason.clone()), false),
                        };
                        if asks && !turn.cancelling {
                            chunks.push(UiChunk::ToolApprovalRequest {
                                approval_id: approval_id(&id),
                                tool_call_id: id.clone(),
                                reason: None,
                                is_automatic: None,
                            });
                        }
                        turn.calls.push(Call { id, reason });
                    }
                }
            }
            Record::Approved { call, by } => {
                if let Some(slot) = self.turn.as_ref().and_then(|turn| turn.calls.get(call.0)) {
                    let id = slot.id.clone();
                    if matches!(by, Approver::Policy) {
                        chunks.push(UiChunk::ToolApprovalRequest {
                            approval_id: approval_id(&id),
                            tool_call_id: id.clone(),
                            reason: Some("read-only".to_owned()),
                            is_automatic: Some(true),
                        });
                    }
                    chunks.push(UiChunk::ToolApprovalResponse {
                        approval_id: approval_id(&id),
                        approved: true,
                    });
                }
            }
            Record::ToolResult { call, exit, output } => {
                if let Some(slot) = self.turn.as_ref().and_then(|turn| turn.calls.get(call.0)) {
                    let text = output.as_ref().map(|output| output.body.text.clone());
                    result_chunks(slot, *exit, text, &mut chunks);
                }
            }
            Record::CancelRequested { .. } => {
                if let Some(turn) = &mut self.turn {
                    turn.cancelling = true;
                }
            }
            Record::TurnEnded { turn, outcome } => {
                let id = *turn;
                if let Some(state) = self.turn.take()
                    && matches!(state.step, StepState::Open)
                {
                    chunks.push(UiChunk::FinishStep);
                    let reason = match outcome {
                        TurnOutcome::Completed => state.finish,
                        TurnOutcome::Cancelled | TurnOutcome::Failed(_) => FinishReason::Stop,
                    };
                    end_chunks(id, *outcome, reason, &mut chunks);
                } else {
                    end_chunks(id, *outcome, FinishReason::Stop, &mut chunks);
                }
            }
            Record::Dispatched { .. } | Record::EpochBumped { .. } => {}
        }
        chunks
    }
}

fn end_chunks(id: TurnId, outcome: TurnOutcome, finish: FinishReason, chunks: &mut Vec<UiChunk>) {
    let reason = match outcome {
        TurnOutcome::Completed => {
            chunks.push(UiChunk::Finish {
                finish_reason: Some(finish),
            });
            TurnEndReason::Completed
        }
        TurnOutcome::Cancelled => {
            chunks.push(UiChunk::Abort {
                reason: Some("cancelled".to_owned()),
            });
            TurnEndReason::Cancelled
        }
        TurnOutcome::Failed(failure) => {
            chunks.push(UiChunk::Error {
                error_text: failure_text(failure),
            });
            TurnEndReason::Failed
        }
    };
    chunks.push(UiChunk::TurnEnded {
        id,
        payload: TurnEndedData { reason },
    });
}

fn part_chunks(step: u32, part: &ModelPart, turn: &mut Turn, chunks: &mut Vec<UiChunk>) {
    match part {
        ModelPart::TextStart { block } => chunks.push(UiChunk::TextStart {
            id: part_id(step, *block),
        }),
        ModelPart::TextDelta { block, delta } => chunks.push(UiChunk::TextDelta {
            id: part_id(step, *block),
            delta: delta.clone(),
        }),
        ModelPart::TextEnd { block } => chunks.push(UiChunk::TextEnd {
            id: part_id(step, *block),
        }),
        ModelPart::ToolInputStart { id, tool_name } => chunks.push(UiChunk::ToolInputStart {
            tool_call_id: id.clone(),
            tool_name: tool_name.clone(),
        }),
        ModelPart::ToolInputDelta { id, delta } => chunks.push(UiChunk::ToolInputDelta {
            tool_call_id: id.clone(),
            input_text_delta: delta.clone(),
        }),
        ModelPart::ToolInputEnd { .. } => {}
        ModelPart::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => chunks.push(UiChunk::ToolInputAvailable {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            input: input.to_value(),
        }),
        ModelPart::Finish { finish_reason } => turn.finish = *finish_reason,
    }
}

fn result_chunks(call: &Call, exit: Exit, text: Option<String>, chunks: &mut Vec<UiChunk>) {
    let id = call.id.clone();
    match exit {
        Exit::Ok => chunks.push(UiChunk::ToolOutputAvailable {
            tool_call_id: id,
            output: json!({"text": text.unwrap_or_default()}),
        }),
        Exit::Err(Cause::Denied) => {
            chunks.push(UiChunk::ToolApprovalResponse {
                approval_id: approval_id(&id),
                approved: false,
            });
            chunks.push(UiChunk::ToolOutputDenied { tool_call_id: id });
        }
        Exit::Err(Cause::Failed(_) | Cause::Interrupted | Cause::Unknown) => {
            let error_text = text
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| synthesized_text(exit, call.reason.as_deref()));
            chunks.push(UiChunk::ToolOutputError {
                tool_call_id: id,
                error_text,
            });
        }
    }
}
