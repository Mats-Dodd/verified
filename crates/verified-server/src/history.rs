//! The conversation the model is sent, rebuilt from the log.
//!
//! Only what the log says happened is sent: the output of attempts that
//! ended in `StepReset` is dropped, `tool_use` blocks without a result (a
//! turn failed by a model failure) are dropped, and results the core
//! synthesized (denied, interrupted, unknown, invalid, past a limit) are
//! rendered as text from their exit and reason. Consecutive messages of one
//! role merge. Every payload is already its parsed type: nothing here
//! guesses or substitutes.

use verified_domain::id::{CallId, ToolName};
use verified_domain::{Cause, Exit, Fault, Limit, Record, Request};

use crate::model::{Content, Message, ModelRequest, ResultKind, Role};
use crate::payload::{ModelPart, Parsed, ToolInput};

/// One tool call of the current turn, as the history sees it.
struct Call {
    id: CallId,
    reason: Option<String>,
    result: Option<(Exit, Option<String>)>,
}

/// The step being assembled: its last attempt's parts and its calls.
#[derive(Default)]
struct Step {
    parts: Vec<ModelPart>,
    /// Indexes into the turn's calls proposed by this step.
    calls: Vec<usize>,
}

struct Builder {
    messages: Vec<Message>,
    calls: Vec<Call>,
    step: Option<Step>,
}

/// The model-facing text of a result the core synthesized.
#[must_use]
pub fn synthesized_text(exit: Exit, reason: Option<&str>) -> String {
    match exit {
        Exit::Ok => "(no output)".to_owned(),
        Exit::Err(Cause::Denied) => "The user denied this tool call.".to_owned(),
        Exit::Err(Cause::Interrupted) => {
            "Interrupted: the user stopped the turn before this call finished.".to_owned()
        }
        Exit::Err(Cause::Unknown) => "Unknown outcome: the agent restarted while this call was running; it may or may not have taken effect.".to_owned(),
        Exit::Err(Cause::Failed(Fault::Limit(Limit::Calls))) => {
            "Not run: the response proposed more tool calls than one step or turn may make.".to_owned()
        }
        Exit::Err(Cause::Failed(Fault::Limit(Limit::Steps | Limit::Context | Limit::Record))) => {
            "Not run: a limit was reached.".to_owned()
        }
        Exit::Err(Cause::Failed(Fault::Overloaded | Fault::Network | Fault::Other)) => reason.map_or_else(
            || "The tool call failed.".to_owned(),
            |reason| format!("Invalid tool call: {reason}"),
        ),
    }
}

impl Builder {
    fn push(&mut self, role: Role, content: Vec<Content>) {
        if content.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some(last) if last.role == role => last.content.extend(content),
            Some(_) | None => self.messages.push(Message { role, content }),
        }
    }

    /// Ends the open step: its text, the calls that have results, then the
    /// results.
    fn close_step(&mut self) {
        let Some(step) = self.step.take() else {
            return;
        };
        let mut assistant = text_blocks(&step.parts);
        let mut results = Vec::new();
        for part in &step.parts {
            let ModelPart::ToolCall {
                tool_call_id,
                tool_name,
                input,
            } = part
            else {
                continue;
            };
            let call = step
                .calls
                .iter()
                .filter_map(|index| self.calls.get(*index))
                .find(|call| call.id == *tool_call_id);
            let Some(call) = call else {
                continue;
            };
            let Some((exit, output)) = &call.result else {
                continue;
            };
            assistant.push(tool_use(tool_call_id, tool_name, input));
            let text = output
                .clone()
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| synthesized_text(*exit, call.reason.as_deref()));
            let kind = match exit {
                Exit::Ok => ResultKind::Success,
                Exit::Err(_) => ResultKind::Error,
            };
            results.push(Content::ToolResult {
                id: tool_call_id.clone(),
                text,
                kind,
            });
        }
        self.push(Role::Assistant, assistant);
        self.push(Role::User, results);
    }

    fn record(&mut self, record: &Record<Parsed>) {
        match record {
            Record::TurnStarted { text, .. } => {
                self.close_step();
                self.calls.clear();
                self.push(
                    Role::User,
                    vec![Content::Text {
                        text: text.body.clone(),
                    }],
                );
            }
            Record::StepStarted { .. } => match &mut self.step {
                Some(step) if step.calls.is_empty() && step.parts.is_empty() => {}
                Some(_) | None => {
                    self.close_step();
                    self.step = Some(Step::default());
                }
            },
            Record::ModelOutput { parts, .. } => {
                if let Some(step) = &mut self.step {
                    step.parts.extend_from_slice(&parts.body);
                }
            }
            Record::StepReset { .. } => {
                if let Some(step) = &mut self.step {
                    step.parts.clear();
                }
            }
            Record::StepFinished { calls, .. } => {
                if let Some(step) = &mut self.step {
                    for proposal in calls {
                        step.calls.push(self.calls.len());
                        let reason = match &proposal.request {
                            Request::Run { .. } => None,
                            Request::Invalid { reason } => Some(reason.clone()),
                        };
                        self.calls.push(Call {
                            id: proposal.id.clone(),
                            reason,
                            result: None,
                        });
                    }
                }
            }
            Record::ToolResult { call, exit, output } => {
                if let Some(slot) = self.calls.get_mut(call.0) {
                    let text = output.as_ref().map(|output| output.body.text.clone());
                    slot.result = Some((*exit, text));
                }
            }
            Record::TurnEnded { .. } => self.close_step(),
            Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::CancelRequested { .. }
            | Record::EpochBumped { .. } => {}
        }
    }
}

fn text_blocks(parts: &[ModelPart]) -> Vec<Content> {
    let mut blocks: Vec<(u32, String)> = Vec::new();
    for part in parts {
        match part {
            ModelPart::TextStart { block } => blocks.push((*block, String::new())),
            ModelPart::TextDelta { block, delta } => {
                if let Some((_, text)) = blocks.iter_mut().rev().find(|(id, _)| id == block) {
                    text.push_str(delta);
                }
            }
            ModelPart::TextEnd { .. }
            | ModelPart::ToolInputStart { .. }
            | ModelPart::ToolInputDelta { .. }
            | ModelPart::ToolInputEnd { .. }
            | ModelPart::ToolCall { .. }
            | ModelPart::Finish { .. } => {}
        }
    }
    blocks
        .into_iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(_, text)| Content::Text { text })
        .collect()
}

fn tool_use(id: &CallId, name: &ToolName, input: &ToolInput) -> Content {
    Content::ToolUse {
        id: id.clone(),
        name: name.clone(),
        input: input.to_value(),
    }
}

/// The request for the next model call, from every record so far.
#[must_use]
pub fn request(records: &[Record<Parsed>], system: &str, max_tokens: u32) -> ModelRequest {
    let mut builder = Builder {
        messages: Vec::new(),
        calls: Vec::new(),
        step: None,
    };
    for record in records {
        builder.record(record);
    }
    // The step about to run has no parts yet; a previous step closes here.
    if let Some(step) = &builder.step
        && !(step.parts.is_empty() && step.calls.is_empty())
    {
        builder.close_step();
    }
    ModelRequest {
        system: system.to_owned(),
        messages: builder.messages,
        max_tokens,
    }
}
