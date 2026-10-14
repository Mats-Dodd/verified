//! The conversation the model is sent, rebuilt from the log.
//!
//! What the user and others said enters at the step that carried it (its
//! `StepStarted`), after the previous step's results: never between a
//! `tool_use` and its result. Only what the log says happened is sent: the
//! output of attempts that
//! ended in `StepReset` is dropped, `tool_use` blocks without a result (a
//! turn failed by a model failure) are dropped, and results the core
//! synthesized (denied, interrupted, unknown, invalid, past a limit) are
//! rendered as text from their exit and reason. Consecutive messages of one
//! role merge. Every payload is already its parsed type: nothing here
//! guesses or substitutes.

use std::collections::BTreeMap;

use verified_domain::id::{CallId, ToolName};
use verified_domain::{Address, Body, EnvelopeId, Exit, Record, Request, Unstarted};

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
    /// What each envelope delivered or notified says, until a step carries
    /// it.
    mail: BTreeMap<EnvelopeId, String>,
}

/// The model-facing text of a result the core synthesized.
#[must_use]
pub fn synthesized_text(exit: Exit, reason: Option<&str>) -> String {
    match exit {
        Exit::Ok => "(no output)".to_owned(),
        Exit::Failed => "The tool call failed.".to_owned(),
        Exit::NotStarted(Unstarted::Denied) => {
            "Not started: the user denied this tool call.".to_owned()
        }
        Exit::NotStarted(Unstarted::Steered) => {
            "Not started: the user sent a message instead of answering.".to_owned()
        }
        Exit::NotStarted(Unstarted::Cancelled) => {
            "Not started: the user stopped the turn before this call ran.".to_owned()
        }
        Exit::NotStarted(Unstarted::Stopped) => {
            "Not started: the user stopped this actor before this call ran.".to_owned()
        }
        Exit::NotStarted(Unstarted::Refused) => {
            "Not started: the recipient is unknown or stopped.".to_owned()
        }
        Exit::NotStarted(Unstarted::Limit) => {
            "Not started: the response proposed more tool calls than one step or turn may make."
                .to_owned()
        }
        Exit::NotStarted(Unstarted::Invalid) => reason.map_or_else(
            || "Not started: the tool call is invalid.".to_owned(),
            |reason| format!("Invalid tool call: {reason}"),
        ),
        Exit::Unknown => {
            "Unknown outcome: it may or may not have taken effect; look before redoing it."
                .to_owned()
        }
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
                Exit::Failed | Exit::NotStarted(_) | Exit::Unknown => ResultKind::Error,
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
            Record::Delivered { envelope } => {
                if let Body::Text(text) = &envelope.body {
                    let said = match envelope.from {
                        Address::User => text.body.clone(),
                        Address::Runtime | Address::Actor(_) => {
                            format!("[from {}] {}", envelope.from, text.body)
                        }
                    };
                    self.mail.insert(envelope.id, said);
                }
            }
            Record::TaskEnded { task, exit, output } => {
                let output = output
                    .as_ref()
                    .map(|output| output.body.text.clone())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| synthesized_text(*exit, None));
                self.mail.insert(
                    EnvelopeId::Task(*task),
                    format!("[v] task {task} ended: {output}"),
                );
            }
            Record::TurnStarted { .. } => {
                self.close_step();
                self.calls.clear();
            }
            Record::StepStarted { carries, .. } => {
                if !carries.is_empty() {
                    self.close_step();
                    let said = carries
                        .iter()
                        .filter_map(|id| self.mail.remove(id))
                        .map(|text| Content::Text { text })
                        .collect();
                    self.push(Role::User, said);
                }
                match &mut self.step {
                    Some(step) if step.calls.is_empty() && step.parts.is_empty() => {}
                    Some(_) | None => {
                        self.close_step();
                        self.step = Some(Step::default());
                    }
                }
            }
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
            | Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::StopSent { .. }
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
        mail: BTreeMap::new(),
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
    // The tools the actor was born with.
    let tools = records
        .iter()
        .find_map(|record| match record {
            Record::Born { born } => Some(born.tools.clone()),
            Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        })
        .unwrap_or_default();
    ModelRequest {
        system: system.to_owned(),
        messages: builder.messages,
        max_tokens,
        tools,
    }
}
