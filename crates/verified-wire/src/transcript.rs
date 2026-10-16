//! Folds UI chunks into a transcript, as the AI SDK does on the client.
//!
//! The transcript is an actor's conversation: its turns, each with the
//! envelopes its steps carried (a user's message, a message from another
//! actor, a task's notification) and the reply; the envelopes that landed
//! and wait in the mailbox; its tasks; and its latest compaction. An
//! envelope lands
//! (`data-envelope`, or a task's end, which is its own notification) and
//! waits until a step carries it (`data-step-carries`): a message sent
//! while a turn runs steers it, and enters that turn at its next step.
//!
//! The reply of each turn is built the way the AI SDK builds a
//! `UIMessage`: a port of `processUIMessageStream` from `ai@7.0.127`
//! (`packages/ai/src/ui/process-ui-message-stream.ts`, Copyright Vercel, Inc.,
//! Apache License 2.0), by way of q's `src/transcript.rs`, cut to the chunks
//! the daemon emits: message, step, text and tool chunks, tool approvals
//! included. Each turn's reply serializes to exactly the JSON the AI SDK
//! produces; the golden tests in `tests/golden.rs` hold it to fixtures the AI
//! SDK computed.
//!
//! Like the AI SDK (and unlike q), a chunk that refers to a part, tool call or
//! approval that does not exist is an error. The fold is atomic per chunk:
//! every check runs before the first change, so on error the transcript is
//! unchanged. Every collection is bounded by [`WireLimits`].

use std::collections::BTreeMap;

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::WireLimits;
use crate::chunk::{EffectEnd, TurnEndReason, UiChunk};
use crate::partial_json::parse_partial_json;
use verified_domain::id::{ApprovalId, CallId, PartId, TaskId, ToolName, TurnId};
use verified_domain::{Address, EnvelopeId};

/// Why a chunk cannot be folded into the transcript.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    #[error("`{chunk}` arrived before any turn started")]
    BeforeFirstTurn { chunk: &'static str },
    #[error("turn {id} started twice, or while turn {running} runs")]
    TurnStartedTwice { id: TurnId, running: TurnId },
    #[error("a step of turn {turn} carries envelope {envelope:?}, which is not in the mailbox")]
    NotQueued { turn: TurnId, envelope: EnvelopeId },
    #[error("task {task} {what}")]
    Task { task: TaskId, what: &'static str },
    #[error("a mailbox, or a turn's inputs, hold at most {limit} envelopes")]
    TooManyEnvelopes { limit: usize },
    #[error("a transcript holds at most {limit} tasks")]
    TooManyTasks { limit: usize },
    #[error("`{chunk}` names turn {found}, but the latest turn is {expected}")]
    TurnMismatch {
        chunk: &'static str,
        expected: TurnId,
        found: TurnId,
    },
    #[error("`{chunk}` names part {id}, which is not streaming")]
    NoActivePart { chunk: &'static str, id: PartId },
    #[error("`{chunk}` names tool call {call_id}, which has no part")]
    NoToolCall {
        chunk: &'static str,
        call_id: CallId,
    },
    #[error("no tool call has approval {approval_id}")]
    NoApproval { approval_id: ApprovalId },
    #[error("a transcript holds at most {limit} turns")]
    TooManyTurns { limit: usize },
    #[error("a reply holds at most {limit} parts")]
    TooManyParts { limit: usize },
    #[error("a part's text is at most {limit} bytes")]
    PartTooLarge { limit: usize },
    #[error("a turn records at most {limit} errors")]
    TooManyErrors { limit: usize },
}

/// The bounds the fold enforces, taken from [`WireLimits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FoldLimits {
    turns: usize,
    parts: usize,
    part_bytes: usize,
    errors: usize,
    envelopes: usize,
    tasks: usize,
}

/// Whether a text part is still streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PartState {
    Streaming,
    Done,
}

/// Where a tool invocation is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolState {
    InputStreaming,
    InputAvailable,
    ApprovalRequested,
    ApprovalResponded,
    OutputAvailable,
    OutputError,
    OutputDenied,
}

/// A tool invocation's approval, as the AI SDK records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: ApprovalId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_automatic: Option<bool>,
}

/// A tool invocation part (the AI SDK's `ToolUIPart`), typed `tool-<name>`.
///
/// Fields mirror the AI SDK's: each state change overwrites `input`,
/// `output`, `errorText` and `rawInput`, and keeps the name and the approval.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolPart {
    pub tool_name: ToolName,
    pub tool_call_id: CallId,
    pub state: ToolState,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub raw_input: Option<Value>,
    pub error_text: Option<String>,
    pub approval: Option<Approval>,
}

impl ToolPart {
    const fn new(tool_name: ToolName, tool_call_id: CallId, state: ToolState) -> Self {
        Self {
            tool_name,
            tool_call_id,
            state,
            input: None,
            output: None,
            raw_input: None,
            error_text: None,
            approval: None,
        }
    }
}

impl Serialize for ToolPart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct View<'turn> {
            #[serde(rename = "type")]
            part_type: String,
            tool_call_id: &'turn CallId,
            state: ToolState,
            #[serde(skip_serializing_if = "Option::is_none")]
            input: Option<&'turn Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output: Option<&'turn Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_input: Option<&'turn Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            error_text: Option<&'turn String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            approval: Option<&'turn Approval>,
        }
        View {
            part_type: format!("tool-{}", self.tool_name),
            tool_call_id: &self.tool_call_id,
            state: self.state,
            input: self.input.as_ref(),
            output: self.output.as_ref(),
            raw_input: self.raw_input.as_ref(),
            error_text: self.error_text.as_ref(),
            approval: self.approval.as_ref(),
        }
        .serialize(serializer)
    }
}

/// A part of an assistant message, as the AI SDK's `UIMessagePart`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Part {
    StepStart,
    Text {
        text: String,
        state: PartState,
    },
    /// Serialized by [`ToolPart`] itself, whose `type` depends on the tool.
    #[serde(untagged)]
    Tool(Box<ToolPart>),
}

/// The assistant's reply as last written (the AI SDK's `write()`): what the
/// AI SDK would show.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Reply<'turn> {
    pub id: &'turn str,
    pub role: &'static str,
    pub parts: &'turn [Part],
}

/// Whether the AI SDK has written (emitted) the message yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Snapshot {
    Unwritten,
    /// Written; `unwritten_steps` `step-start` parts were appended since,
    /// which the AI SDK adds without writing.
    Written {
        unwritten_steps: usize,
    },
}

/// A tool call whose input is streaming (the AI SDK's `partialToolCalls`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PartialToolCall {
    text: String,
    tool_name: ToolName,
}

/// The reply being built: the AI SDK's streaming message state.
#[derive(Debug, Clone)]
struct ReplyBuilder {
    id: String,
    parts: Vec<Part>,
    active_text: BTreeMap<PartId, usize>,
    partial_tool_calls: BTreeMap<CallId, PartialToolCall>,
    snapshot: Snapshot,
}

impl ReplyBuilder {
    const fn new(id: String) -> Self {
        Self {
            id,
            parts: Vec::new(),
            active_text: BTreeMap::new(),
            partial_tool_calls: BTreeMap::new(),
            snapshot: Snapshot::Unwritten,
        }
    }

    fn reply(&self) -> Option<Reply<'_>> {
        let Snapshot::Written { unwritten_steps } = self.snapshot else {
            return None;
        };
        let shown = self.parts.len().saturating_sub(unwritten_steps);
        Some(Reply {
            id: &self.id,
            role: "assistant",
            parts: self.parts.get(..shown).unwrap_or_default(),
        })
    }

    const fn write(&mut self) {
        self.snapshot = Snapshot::Written { unwritten_steps: 0 };
    }

    const fn room(&self, limits: FoldLimits) -> Result<(), FoldError> {
        if self.parts.len() < limits.parts {
            Ok(())
        } else {
            Err(FoldError::TooManyParts {
                limit: limits.parts,
            })
        }
    }

    fn push(&mut self, part: Part, limits: FoldLimits) -> Result<(), FoldError> {
        self.room(limits)?;
        self.parts.push(part);
        self.write();
        Ok(())
    }

    /// The index of the current step's first part: everything after the last `step-start`.
    fn step_start(&self) -> usize {
        self.parts
            .iter()
            .rposition(|part| matches!(part, Part::StepStart))
            .map_or(0, |index| index.saturating_add(1))
    }

    /// A tool part of the current step with this call id.
    fn tool_in_step(&self, call_id: &CallId) -> Option<usize> {
        let start = self.step_start();
        self.parts
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, part)| match part {
                Part::Tool(tool) if tool.tool_call_id == *call_id => Some(index),
                Part::Tool(_) | Part::StepStart | Part::Text { .. } => None,
            })
    }

    /// The AI SDK's `getToolInvocation`: the current step's, else the latest anywhere.
    fn tool_invocation(&self, call_id: &CallId) -> Option<usize> {
        self.tool_in_step(call_id).or_else(|| {
            self.parts
                .iter()
                .rposition(|part| matches!(part, Part::Tool(tool) if tool.tool_call_id == *call_id))
        })
    }

    fn tool_by_approval(&self, approval_id: &ApprovalId) -> Option<usize> {
        self.parts.iter().position(|part| {
            matches!(part, Part::Tool(tool) if tool.approval.as_ref().is_some_and(|approval| approval.id == *approval_id))
        })
    }

    fn tool_mut(&mut self, index: usize) -> Option<&mut ToolPart> {
        match self.parts.get_mut(index) {
            Some(Part::Tool(tool)) => Some(&mut **tool),
            Some(Part::StepStart | Part::Text { .. }) | None => None,
        }
    }

    fn tool(&self, index: usize) -> Option<&ToolPart> {
        match self.parts.get(index) {
            Some(Part::Tool(tool)) => Some(&**tool),
            Some(Part::StepStart | Part::Text { .. }) | None => None,
        }
    }

    /// The AI SDK's `updateToolPart`: replaces the part at `existing`, else
    /// the current step's part for the call, keeping its name and approval;
    /// with neither, adds `part`.
    fn update_tool(
        &mut self,
        part: ToolPart,
        existing: Option<usize>,
        limits: FoldLimits,
    ) -> Result<(), FoldError> {
        let index = existing.or_else(|| self.tool_in_step(&part.tool_call_id));
        if let Some(tool) = index.and_then(|index| self.tool_mut(index)) {
            *tool = ToolPart {
                tool_name: tool.tool_name.clone(),
                approval: tool.approval.take(),
                ..part
            };
            Ok(())
        } else {
            self.room(limits)?;
            self.parts.push(Part::Tool(Box::new(part)));
            Ok(())
        }
    }

    fn missing_tool(chunk: &UiChunk, call_id: &CallId) -> FoldError {
        FoldError::NoToolCall {
            chunk: chunk.kind(),
            call_id: call_id.clone(),
        }
    }

    /// Appends `delta` to `text` unless that passes the part size bound.
    fn extend_text(text: &mut String, delta: &str, limits: FoldLimits) -> Result<(), FoldError> {
        if text.len().saturating_add(delta.len()) > limits.part_bytes {
            return Err(FoldError::PartTooLarge {
                limit: limits.part_bytes,
            });
        }
        text.push_str(delta);
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per chunk type, like the original's switch"
    )]
    fn apply(&mut self, chunk: &UiChunk, limits: FoldLimits) -> Result<(), FoldError> {
        match chunk {
            UiChunk::TextStart { id } => {
                self.push(
                    Part::Text {
                        text: String::new(),
                        state: PartState::Streaming,
                    },
                    limits,
                )?;
                let index = self.parts.len().saturating_sub(1);
                self.active_text.insert(id.clone(), index);
            }
            UiChunk::TextDelta { id, delta } => {
                let index = self.active_text(chunk, id)?;
                if let Some(Part::Text { text, .. }) = self.parts.get_mut(index) {
                    Self::extend_text(text, delta, limits)?;
                }
                self.write();
            }
            UiChunk::TextEnd { id } => {
                let index = self.active_text(chunk, id)?;
                if let Some(Part::Text { state, .. }) = self.parts.get_mut(index) {
                    *state = PartState::Done;
                }
                self.active_text.remove(id);
                self.write();
            }
            UiChunk::ToolInputStart {
                tool_call_id,
                tool_name,
            } => {
                let part = ToolPart::new(
                    tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::InputStreaming,
                );
                self.update_tool(part, None, limits)?;
                self.partial_tool_calls.insert(
                    tool_call_id.clone(),
                    PartialToolCall {
                        text: String::new(),
                        tool_name: tool_name.clone(),
                    },
                );
                self.write();
            }
            UiChunk::ToolInputDelta {
                tool_call_id,
                input_text_delta,
            } => {
                let partial = self
                    .partial_tool_calls
                    .get(tool_call_id)
                    .ok_or_else(|| Self::missing_tool(chunk, tool_call_id))?;
                let mut text = partial.text.clone();
                Self::extend_text(&mut text, input_text_delta, limits)?;
                let mut part = ToolPart::new(
                    partial.tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::InputStreaming,
                );
                part.input = parse_partial_json(&text);
                part.raw_input = Some(Value::String(text.clone()));
                self.update_tool(part, None, limits)?;
                if let Some(partial) = self.partial_tool_calls.get_mut(tool_call_id) {
                    partial.text = text;
                }
                self.write();
            }
            UiChunk::ToolInputAvailable {
                tool_call_id,
                tool_name,
                input,
            } => {
                let mut part = ToolPart::new(
                    tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::InputAvailable,
                );
                part.input = Some(input.clone());
                self.update_tool(part, None, limits)?;
                self.write();
            }
            UiChunk::ToolApprovalRequest {
                approval_id,
                tool_call_id,
                reason,
                is_automatic,
            } => {
                let index = self
                    .tool_invocation(tool_call_id)
                    .ok_or_else(|| Self::missing_tool(chunk, tool_call_id))?;
                if let Some(tool) = self.tool_mut(index) {
                    tool.state = ToolState::ApprovalRequested;
                    tool.approval = Some(Approval {
                        id: approval_id.clone(),
                        approved: None,
                        request_reason: reason.clone(),
                        is_automatic: is_automatic.filter(|automatic| *automatic),
                    });
                }
                self.write();
            }
            UiChunk::ToolApprovalResponse {
                approval_id,
                approved,
            } => {
                let index =
                    self.tool_by_approval(approval_id)
                        .ok_or_else(|| FoldError::NoApproval {
                            approval_id: approval_id.clone(),
                        })?;
                if let Some(tool) = self.tool_mut(index) {
                    tool.state = ToolState::ApprovalResponded;
                    if let Some(approval) = &mut tool.approval {
                        approval.approved = Some(*approved);
                    }
                }
                self.write();
            }
            UiChunk::ToolOutputDenied { tool_call_id } => {
                let index = self
                    .tool_invocation(tool_call_id)
                    .ok_or_else(|| Self::missing_tool(chunk, tool_call_id))?;
                if let Some(tool) = self.tool_mut(index) {
                    tool.state = ToolState::OutputDenied;
                }
                self.write();
            }
            UiChunk::ToolOutputAvailable {
                tool_call_id,
                output,
            } => {
                let (index, tool) = self.existing_tool(chunk, tool_call_id)?;
                let mut part = ToolPart::new(
                    tool.tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::OutputAvailable,
                );
                part.input.clone_from(&tool.input);
                part.output = Some(output.clone());
                self.update_tool(part, Some(index), limits)?;
                self.write();
            }
            UiChunk::ToolOutputError {
                tool_call_id,
                error_text,
            } => {
                let (index, tool) = self.existing_tool(chunk, tool_call_id)?;
                let mut part = ToolPart::new(
                    tool.tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::OutputError,
                );
                part.input.clone_from(&tool.input);
                part.raw_input.clone_from(&tool.raw_input);
                part.error_text = Some(error_text.clone());
                self.update_tool(part, Some(index), limits)?;
                self.write();
            }
            UiChunk::StartStep => {
                self.room(limits)?;
                self.parts.push(Part::StepStart);
                if let Snapshot::Written { unwritten_steps } = &mut self.snapshot {
                    *unwritten_steps = unwritten_steps.saturating_add(1);
                }
            }
            UiChunk::ResetStep => {
                self.active_text.clear();
                self.partial_tool_calls.clear();
                let start = self.step_start();
                if start < self.parts.len() {
                    self.parts.truncate(start);
                    self.write();
                }
            }
            UiChunk::Start { message_id } => {
                if let Some(id) = message_id {
                    id.clone_into(&mut self.id);
                    self.write();
                }
            }
            // Steps end and messages finish or abort without changing the
            // message; errors are the turn's, and actor data the
            // transcript's (see `TranscriptTurn` and `Transcript`).
            UiChunk::FinishStep
            | UiChunk::Finish { .. }
            | UiChunk::Abort { .. }
            | UiChunk::Error { .. }
            | UiChunk::Envelope { .. }
            | UiChunk::TurnStarted { .. }
            | UiChunk::TurnEnded { .. }
            | UiChunk::StepCarries { .. }
            | UiChunk::TaskStarted { .. }
            | UiChunk::TaskEnded { .. }
            | UiChunk::Compacted { .. } => {}
        }
        Ok(())
    }

    /// The tool part a result chunk names, and a copy of it.
    fn existing_tool(
        &self,
        chunk: &UiChunk,
        call_id: &CallId,
    ) -> Result<(usize, ToolPart), FoldError> {
        self.tool_invocation(call_id)
            .and_then(|index| self.tool(index).map(|tool| (index, tool.clone())))
            .ok_or_else(|| Self::missing_tool(chunk, call_id))
    }

    /// The index of the streaming text part `id`.
    fn active_text(&self, chunk: &UiChunk, id: &PartId) -> Result<usize, FoldError> {
        self.active_text
            .get(id)
            .copied()
            .ok_or_else(|| FoldError::NoActivePart {
                chunk: chunk.kind(),
                id: id.clone(),
            })
    }
}

/// An envelope that landed in the mailbox: from whom, and what it says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mail {
    pub envelope: EnvelopeId,
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Address>,
    pub text: String,
}

/// An envelope a step of a turn carried to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    #[serde(flatten)]
    pub mail: Mail,
    /// The step that carried it: the number of `step-start` parts of the
    /// reply before it. 0 started the turn; a later one steered it.
    pub step: usize,
}

/// A task as its stream tells it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub tool: ToolName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// How it ended and what it said; `None` while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<(EffectEnd, String)>,
}

/// The latest compaction of the model's history: what the model is sent in
/// place of everything before it. The transcript keeps all of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compaction {
    pub summary: String,
    pub bytes: u64,
    /// The turns started before it: it follows the last of them.
    pub turns: usize,
}

/// One turn: what its steps carried and everything the stream says about
/// the reply.
#[derive(Debug, Clone)]
pub struct TranscriptTurn {
    id: TurnId,
    inputs: Vec<Input>,
    model: String,
    errors: Vec<String>,
    end: Option<TurnEndReason>,
    revision: u64,
    draft: ReplyBuilder,
}

impl TranscriptTurn {
    const fn new(id: TurnId, model: String) -> Self {
        Self {
            id,
            inputs: Vec::new(),
            model,
            errors: Vec::new(),
            end: None,
            revision: 0,
            // `readUIMessageStream` starts every message with an empty id;
            // the `start` chunk names it.
            draft: ReplyBuilder::new(String::new()),
        }
    }

    #[must_use]
    pub const fn id(&self) -> TurnId {
        self.id
    }

    /// What the turn's steps carried, in order: the first step's started
    /// it, later ones steered it.
    #[must_use]
    pub fn inputs(&self) -> &[Input] {
        &self.inputs
    }

    /// The model the turn runs on.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Every error reported during the turn.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// How the turn ended; `None` while it runs.
    #[must_use]
    pub const fn end(&self) -> Option<TurnEndReason> {
        self.end
    }

    /// Bumped on every applied chunk, for render caches.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The reply so far; `None` until the model has said anything.
    #[must_use]
    pub fn reply(&self) -> Option<Reply<'_>> {
        self.draft.reply()
    }

    fn check_turn(&self, chunk: &UiChunk, id: TurnId) -> Result<(), FoldError> {
        if id == self.id {
            Ok(())
        } else {
            Err(FoldError::TurnMismatch {
                chunk: chunk.kind(),
                expected: self.id,
                found: id,
            })
        }
    }

    /// The step a carried envelope enters at: the reply's `step-start`s
    /// so far.
    fn steps(&self) -> usize {
        self.draft
            .parts
            .iter()
            .filter(|part| matches!(part, Part::StepStart))
            .count()
    }

    const fn bump(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("a turn sees fewer than u64::MAX chunks");
    }

    fn apply(&mut self, chunk: &UiChunk, limits: FoldLimits) -> Result<(), FoldError> {
        if let UiChunk::TurnEnded { id, payload } = chunk {
            self.check_turn(chunk, *id)?;
            self.end = Some(payload.reason);
        } else if let UiChunk::Error { error_text } = chunk {
            if self.errors.len() >= limits.errors {
                return Err(FoldError::TooManyErrors {
                    limit: limits.errors,
                });
            }
            self.errors.push(error_text.clone());
        } else {
            self.draft.apply(chunk, limits)?;
        }
        self.bump();
        Ok(())
    }
}

impl Serialize for TranscriptTurn {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'turn> {
            id: &'turn TurnId,
            inputs: &'turn [Input],
            reply: Option<Reply<'turn>>,
            errors: &'turn [String],
            end: Option<TurnEndReason>,
        }
        View {
            id: &self.id,
            inputs: &self.inputs,
            reply: self.reply(),
            errors: &self.errors,
            end: self.end,
        }
        .serialize(serializer)
    }
}

/// An actor's conversation so far: its turns in order, the mail waiting,
/// and its tasks.
#[derive(Debug, Clone)]
pub struct Transcript {
    turns: Vec<TranscriptTurn>,
    queued: Vec<Mail>,
    tasks: BTreeMap<TaskId, TaskView>,
    compacted: Option<Compaction>,
    stopped: bool,
    limits: FoldLimits,
}

impl Transcript {
    #[must_use]
    pub fn new(limits: &WireLimits) -> Self {
        Self {
            turns: Vec::with_capacity(limits.max_turns),
            queued: Vec::new(),
            tasks: BTreeMap::new(),
            compacted: None,
            stopped: false,
            limits: FoldLimits {
                turns: limits.max_turns,
                parts: limits.max_parts_per_reply,
                part_bytes: limits.max_part_bytes,
                errors: limits.max_errors_per_turn,
                envelopes: limits.max_envelopes,
                tasks: limits.max_tasks,
            },
        }
    }

    #[must_use]
    pub fn turns(&self) -> &[TranscriptTurn] {
        &self.turns
    }

    /// The turn still in progress, if any.
    #[must_use]
    pub fn running(&self) -> Option<&TranscriptTurn> {
        self.turns.last().filter(|turn| turn.end.is_none())
    }

    /// What landed and no step has carried yet, in arrival order.
    #[must_use]
    pub fn queued(&self) -> &[Mail] {
        &self.queued
    }

    /// Every task the actor started, by id.
    #[must_use]
    pub const fn tasks(&self) -> &BTreeMap<TaskId, TaskView> {
        &self.tasks
    }

    /// The latest compaction, if the history was ever compacted.
    #[must_use]
    pub const fn compacted(&self) -> Option<&Compaction> {
        self.compacted.as_ref()
    }

    /// A stop landed: the actor starts nothing more.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stopped
    }

    fn queue(&mut self, mail: Mail) -> Result<(), FoldError> {
        if self.queued.len() >= self.limits.envelopes {
            return Err(FoldError::TooManyEnvelopes {
                limit: self.limits.envelopes,
            });
        }
        self.queued.push(mail);
        Ok(())
    }

    /// Moves `envelopes` from the mailbox into the running turn `id`, at
    /// its next step. Every check runs before the first change.
    fn carry(
        &mut self,
        chunk: &UiChunk,
        id: TurnId,
        envelopes: &[EnvelopeId],
    ) -> Result<(), FoldError> {
        let limits = self.limits;
        let turn = self.turns.last_mut().ok_or(FoldError::BeforeFirstTurn {
            chunk: chunk.kind(),
        })?;
        turn.check_turn(chunk, id)?;
        if turn.inputs.len().saturating_add(envelopes.len()) > limits.envelopes {
            return Err(FoldError::TooManyEnvelopes {
                limit: limits.envelopes,
            });
        }
        let mut positions = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            let position = self
                .queued
                .iter()
                .position(|mail| mail.envelope == *envelope)
                .filter(|position| !positions.contains(position))
                .ok_or(FoldError::NotQueued {
                    turn: id,
                    envelope: *envelope,
                })?;
            positions.push(position);
        }
        let step = turn.steps();
        let mut taken: Vec<Option<Mail>> = self.queued.drain(..).map(Some).collect();
        for position in positions {
            if let Some(mail) = taken.get_mut(position).and_then(Option::take) {
                turn.inputs.push(Input { mail, step });
            }
        }
        self.queued = taken.into_iter().flatten().collect();
        turn.bump();
        Ok(())
    }

    /// Folds one chunk in.
    ///
    /// # Errors
    /// A chunk that does not fit the transcript (see [`FoldError`]). The
    /// transcript is unchanged; the stream it came from is broken.
    pub fn apply(&mut self, chunk: &UiChunk) -> Result<(), FoldError> {
        match chunk {
            UiChunk::Envelope { payload } => {
                match &payload.text {
                    Some(text) => self.queue(Mail {
                        envelope: payload.envelope,
                        from: payload.from,
                        reply_to: payload.reply_to,
                        text: text.clone(),
                    })?,
                    None => self.stopped = true,
                }
                Ok(())
            }
            UiChunk::TurnStarted { id, payload } => {
                if let Some(running) = self
                    .turns
                    .last()
                    .filter(|turn| turn.end.is_none() || turn.id >= *id)
                {
                    return Err(FoldError::TurnStartedTwice {
                        id: *id,
                        running: running.id,
                    });
                }
                if self.turns.len() >= self.limits.turns {
                    return Err(FoldError::TooManyTurns {
                        limit: self.limits.turns,
                    });
                }
                self.turns
                    .push(TranscriptTurn::new(*id, payload.model.clone()));
                Ok(())
            }
            UiChunk::StepCarries { id, payload } => self.carry(chunk, *id, &payload.envelopes),
            UiChunk::Compacted { payload } => {
                if payload.summary.len() > self.limits.part_bytes {
                    return Err(FoldError::PartTooLarge {
                        limit: self.limits.part_bytes,
                    });
                }
                self.compacted = Some(Compaction {
                    summary: payload.summary.clone(),
                    bytes: payload.bytes,
                    turns: self.turns.len(),
                });
                Ok(())
            }
            UiChunk::TaskStarted { id, payload } => {
                if self.tasks.contains_key(id) {
                    return Err(FoldError::Task {
                        task: *id,
                        what: "started twice",
                    });
                }
                if self.tasks.len() >= self.limits.tasks {
                    return Err(FoldError::TooManyTasks {
                        limit: self.limits.tasks,
                    });
                }
                self.tasks.insert(
                    *id,
                    TaskView {
                        tool: payload.tool.clone(),
                        subject: payload.subject.clone(),
                        end: None,
                    },
                );
                Ok(())
            }
            UiChunk::TaskEnded { id, payload } => {
                match self.tasks.get(id) {
                    None => {
                        return Err(FoldError::Task {
                            task: *id,
                            what: "ended but never started",
                        });
                    }
                    Some(TaskView { end: Some(_), .. }) => {
                        return Err(FoldError::Task {
                            task: *id,
                            what: "ended twice",
                        });
                    }
                    Some(TaskView { end: None, .. }) => {}
                }
                // The end is also the notification, from `v`.
                self.queue(Mail {
                    envelope: EnvelopeId::Task(*id),
                    from: Address::Runtime,
                    reply_to: None,
                    text: format!("task {id} ended: {}", payload.end),
                })?;
                if let Some(task) = self.tasks.get_mut(id) {
                    task.end = Some((payload.end, payload.text.clone()));
                }
                Ok(())
            }
            UiChunk::Start { .. }
            | UiChunk::Finish { .. }
            | UiChunk::Abort { .. }
            | UiChunk::Error { .. }
            | UiChunk::StartStep
            | UiChunk::FinishStep
            | UiChunk::ResetStep
            | UiChunk::TextStart { .. }
            | UiChunk::TextDelta { .. }
            | UiChunk::TextEnd { .. }
            | UiChunk::ToolInputStart { .. }
            | UiChunk::ToolInputDelta { .. }
            | UiChunk::ToolInputAvailable { .. }
            | UiChunk::ToolApprovalRequest { .. }
            | UiChunk::ToolApprovalResponse { .. }
            | UiChunk::ToolOutputAvailable { .. }
            | UiChunk::ToolOutputError { .. }
            | UiChunk::ToolOutputDenied { .. }
            | UiChunk::TurnEnded { .. } => {
                let limits = self.limits;
                let turn = self.turns.last_mut().ok_or(FoldError::BeforeFirstTurn {
                    chunk: chunk.kind(),
                })?;
                turn.apply(chunk, limits)
            }
        }
    }
}
