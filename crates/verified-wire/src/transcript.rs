//! Folds UI chunks into a transcript, as the AI SDK does on the client.
//!
//! The reply to each user message is built the way the AI SDK builds a
//! `UIMessage`: a port of `processUIMessageStream` from `ai@7.0.127`
//! (`packages/ai/src/ui/process-ui-message-stream.ts`, Copyright Vercel, Inc.,
//! Apache License 2.0), by way of q's `src/transcript.rs`. It covers message,
//! step, text, reasoning, file, source, custom and tool chunks, tool approvals
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
use crate::chunk::{JsonObject, ProviderMetadata, TurnEndReason, UiChunk};
use crate::partial_json::parse_partial_json;
use verified_domain::id::{ApprovalId, CallId, MessageId, PartId, ToolName, TurnId};

/// Why a chunk cannot be folded into the transcript.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    #[error("`{chunk}` arrived before any user message")]
    BeforeFirstTurn { chunk: &'static str },
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
}

/// Whether a text or reasoning part is still streaming.
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

/// A static tool part is typed `tool-<name>`; a dynamic one is
/// `dynamic-tool` with a `toolName` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Static,
    Dynamic,
}

impl ToolKind {
    const fn of(dynamic: Option<bool>) -> Self {
        match dynamic {
            Some(true) => Self::Dynamic,
            Some(false) | None => Self::Static,
        }
    }
}

/// A tool invocation's approval, as the AI SDK records it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: ApprovalId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_automatic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema_input: Option<Value>,
}

/// A tool invocation part (the AI SDK's `ToolUIPart` / `DynamicToolUIPart`).
///
/// Fields mirror the AI SDK's: each state change overwrites `input`,
/// `output`, `errorText`, `rawInput` and `preliminary`, keeps `title` and
/// `toolMetadata` unless given new ones, and keeps `providerExecuted` once set.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolPart {
    pub kind: ToolKind,
    pub tool_name: ToolName,
    pub tool_call_id: CallId,
    pub state: ToolState,
    pub title: Option<String>,
    pub tool_metadata: Option<JsonObject>,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub raw_input: Option<Value>,
    pub error_text: Option<String>,
    pub provider_executed: Option<bool>,
    pub preliminary: Option<bool>,
    pub call_provider_metadata: Option<ProviderMetadata>,
    pub result_provider_metadata: Option<ProviderMetadata>,
    pub approval: Option<Approval>,
}

impl Serialize for ToolPart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct View<'turn> {
            #[serde(rename = "type")]
            part_type: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_name: Option<&'turn ToolName>,
            tool_call_id: &'turn CallId,
            state: ToolState,
            #[serde(skip_serializing_if = "Option::is_none")]
            title: Option<&'turn String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_metadata: Option<&'turn JsonObject>,
            #[serde(skip_serializing_if = "Option::is_none")]
            input: Option<&'turn Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output: Option<&'turn Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_input: Option<&'turn Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            error_text: Option<&'turn String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            provider_executed: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            preliminary: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            call_provider_metadata: Option<&'turn ProviderMetadata>,
            #[serde(skip_serializing_if = "Option::is_none")]
            result_provider_metadata: Option<&'turn ProviderMetadata>,
            #[serde(skip_serializing_if = "Option::is_none")]
            approval: Option<&'turn Approval>,
        }
        let (part_type, tool_name) = match self.kind {
            ToolKind::Static => (format!("tool-{}", self.tool_name), None),
            ToolKind::Dynamic => ("dynamic-tool".to_owned(), Some(&self.tool_name)),
        };
        View {
            part_type,
            tool_name,
            tool_call_id: &self.tool_call_id,
            state: self.state,
            title: self.title.as_ref(),
            tool_metadata: self.tool_metadata.as_ref(),
            input: self.input.as_ref(),
            output: self.output.as_ref(),
            raw_input: self.raw_input.as_ref(),
            error_text: self.error_text.as_ref(),
            provider_executed: self.provider_executed,
            preliminary: self.preliminary,
            call_provider_metadata: self.call_provider_metadata.as_ref(),
            result_provider_metadata: self.result_provider_metadata.as_ref(),
            approval: self.approval.as_ref(),
        }
        .serialize(serializer)
    }
}

/// A part of an assistant message, as the AI SDK's `UIMessagePart`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Part {
    StepStart,
    Text {
        text: String,
        state: PartState,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Reasoning {
        id: PartId,
        text: String,
        state: PartState,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    File {
        media_type: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningFile {
        media_type: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    SourceUrl {
        source_id: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    SourceDocument {
        source_id: String,
        media_type: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Custom {
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'turn Value>,
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
#[derive(Debug, Clone, PartialEq)]
struct PartialToolCall {
    text: String,
    kind: ToolKind,
    tool_name: ToolName,
    title: Option<String>,
    tool_metadata: Option<JsonObject>,
}

/// Which tool parts a lookup matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Kind(ToolKind),
    Any,
}

/// One tool part change: the options of the AI SDK's `updateToolPart`.
#[derive(Debug)]
struct ToolUpdate {
    kind: ToolKind,
    tool_name: ToolName,
    tool_call_id: CallId,
    state: ToolState,
    input: Option<Value>,
    output: Option<Value>,
    raw_input: Option<Value>,
    error_text: Option<String>,
    preliminary: Option<bool>,
    provider_executed: Option<bool>,
    provider_metadata: Option<ProviderMetadata>,
    title: Option<String>,
    tool_metadata: Option<JsonObject>,
}

impl ToolUpdate {
    const fn new(
        kind: ToolKind,
        tool_name: ToolName,
        tool_call_id: CallId,
        state: ToolState,
    ) -> Self {
        Self {
            kind,
            tool_name,
            tool_call_id,
            state,
            input: None,
            output: None,
            raw_input: None,
            error_text: None,
            preliminary: None,
            provider_executed: None,
            provider_metadata: None,
            title: None,
            tool_metadata: None,
        }
    }

    const fn is_result(&self) -> bool {
        matches!(
            self.state,
            ToolState::OutputAvailable | ToolState::OutputError
        )
    }

    fn into_part(self) -> ToolPart {
        let is_result = self.is_result();
        let (call_provider_metadata, result_provider_metadata) = if is_result {
            (None, self.provider_metadata)
        } else {
            (self.provider_metadata, None)
        };
        ToolPart {
            kind: self.kind,
            tool_name: self.tool_name,
            tool_call_id: self.tool_call_id,
            state: self.state,
            title: self.title,
            tool_metadata: self.tool_metadata,
            input: self.input,
            output: self.output,
            // A new dynamic part leaves `rawInput` out, as the AI SDK does.
            raw_input: match self.kind {
                ToolKind::Static => self.raw_input,
                ToolKind::Dynamic => None,
            },
            error_text: self.error_text,
            provider_executed: self.provider_executed,
            preliminary: self.preliminary,
            call_provider_metadata,
            result_provider_metadata,
            approval: None,
        }
    }
}

impl ToolPart {
    fn update(&mut self, update: ToolUpdate) {
        let is_result = update.is_result();
        self.state = update.state;
        if self.kind == ToolKind::Dynamic {
            self.tool_name = update.tool_name;
        }
        self.input = update.input;
        self.output = update.output;
        self.error_text = update.error_text;
        self.raw_input = update.raw_input;
        self.preliminary = update.preliminary;
        if update.title.is_some() {
            self.title = update.title;
        }
        if update.tool_metadata.is_some() {
            self.tool_metadata = update.tool_metadata;
        }
        self.provider_executed = update.provider_executed.or(self.provider_executed);
        if let Some(metadata) = update.provider_metadata {
            if is_result {
                self.result_provider_metadata = Some(metadata);
            } else {
                self.call_provider_metadata = Some(metadata);
            }
        }
    }
}

/// The reply being built: the AI SDK's streaming message state.
#[derive(Debug, Clone)]
struct ReplyBuilder {
    id: String,
    metadata: Option<Value>,
    parts: Vec<Part>,
    active_text: BTreeMap<PartId, usize>,
    active_reasoning: BTreeMap<PartId, usize>,
    partial_tool_calls: BTreeMap<CallId, PartialToolCall>,
    snapshot: Snapshot,
}

impl ReplyBuilder {
    const fn new(id: String) -> Self {
        Self {
            id,
            metadata: None,
            parts: Vec::new(),
            active_text: BTreeMap::new(),
            active_reasoning: BTreeMap::new(),
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
            metadata: self.metadata.as_ref(),
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
    fn tool_in_step(&self, call_id: &CallId, lookup: Lookup) -> Option<usize> {
        let start = self.step_start();
        self.parts
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, part)| match part {
                Part::Tool(tool)
                    if tool.tool_call_id == *call_id
                        && match lookup {
                            Lookup::Kind(kind) => tool.kind == kind,
                            Lookup::Any => true,
                        } =>
                {
                    Some(index)
                }
                Part::Tool(_)
                | Part::StepStart
                | Part::Text { .. }
                | Part::Reasoning { .. }
                | Part::File { .. }
                | Part::ReasoningFile { .. }
                | Part::SourceUrl { .. }
                | Part::SourceDocument { .. }
                | Part::Custom { .. } => None,
            })
    }

    /// The AI SDK's `getToolInvocation`: the current step's, else the latest anywhere.
    fn tool_invocation(&self, call_id: &CallId) -> Option<usize> {
        self.tool_in_step(call_id, Lookup::Any).or_else(|| {
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
            Some(
                Part::StepStart
                | Part::Text { .. }
                | Part::Reasoning { .. }
                | Part::File { .. }
                | Part::ReasoningFile { .. }
                | Part::SourceUrl { .. }
                | Part::SourceDocument { .. }
                | Part::Custom { .. },
            )
            | None => None,
        }
    }

    fn tool(&self, index: usize) -> Option<&ToolPart> {
        match self.parts.get(index) {
            Some(Part::Tool(tool)) => Some(&**tool),
            Some(
                Part::StepStart
                | Part::Text { .. }
                | Part::Reasoning { .. }
                | Part::File { .. }
                | Part::ReasoningFile { .. }
                | Part::SourceUrl { .. }
                | Part::SourceDocument { .. }
                | Part::Custom { .. },
            )
            | None => None,
        }
    }

    /// The AI SDK's `updateToolPart` / `updateDynamicToolPart`.
    fn update_tool(
        &mut self,
        update: ToolUpdate,
        existing: Option<usize>,
        limits: FoldLimits,
    ) -> Result<(), FoldError> {
        let index =
            existing.or_else(|| self.tool_in_step(&update.tool_call_id, Lookup::Kind(update.kind)));
        if let Some(tool) = index.and_then(|index| self.tool_mut(index)) {
            tool.update(update);
            Ok(())
        } else {
            self.room(limits)?;
            self.parts.push(Part::Tool(Box::new(update.into_part())));
            Ok(())
        }
    }

    fn missing_tool(chunk: &UiChunk, call_id: &CallId) -> FoldError {
        FoldError::NoToolCall {
            chunk: chunk.kind(),
            call_id: call_id.clone(),
        }
    }

    /// Merges in message metadata; `true` if there was any.
    fn update_metadata(&mut self, metadata: Option<&Value>) -> bool {
        let Some(metadata) = metadata.filter(|value| !value.is_null()) else {
            return false;
        };
        self.metadata = Some(match self.metadata.take() {
            Some(existing) => merge(existing, metadata),
            None => metadata.clone(),
        });
        true
    }

    fn write_metadata(&mut self, metadata: Option<&Value>) {
        if self.update_metadata(metadata) {
            self.write();
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
            UiChunk::TextStart {
                id,
                provider_metadata,
            } => {
                self.push(
                    Part::Text {
                        text: String::new(),
                        state: PartState::Streaming,
                        provider_metadata: provider_metadata.clone(),
                    },
                    limits,
                )?;
                let index = self.parts.len().saturating_sub(1);
                self.active_text.insert(id.clone(), index);
            }
            UiChunk::TextDelta {
                id,
                delta,
                provider_metadata,
            } => {
                let index = self.active_index(Active::Text, chunk, id)?;
                if let Some(Part::Text {
                    text,
                    provider_metadata: current,
                    ..
                }) = self.parts.get_mut(index)
                {
                    Self::extend_text(text, delta, limits)?;
                    update(current, provider_metadata.as_ref());
                }
                self.write();
            }
            UiChunk::TextEnd {
                id,
                provider_metadata,
            } => {
                let index = self.active_index(Active::Text, chunk, id)?;
                if let Some(Part::Text {
                    state,
                    provider_metadata: current,
                    ..
                }) = self.parts.get_mut(index)
                {
                    *state = PartState::Done;
                    update(current, provider_metadata.as_ref());
                }
                self.active_text.remove(id);
                self.write();
            }
            UiChunk::ReasoningStart {
                id,
                provider_metadata,
            } => {
                self.push(
                    Part::Reasoning {
                        id: id.clone(),
                        text: String::new(),
                        state: PartState::Streaming,
                        provider_metadata: provider_metadata.clone(),
                    },
                    limits,
                )?;
                let index = self.parts.len().saturating_sub(1);
                self.active_reasoning.insert(id.clone(), index);
            }
            UiChunk::ReasoningDelta {
                id,
                delta,
                provider_metadata,
            } => {
                let index = self.active_index(Active::Reasoning, chunk, id)?;
                if let Some(Part::Reasoning {
                    text,
                    provider_metadata: current,
                    ..
                }) = self.parts.get_mut(index)
                {
                    Self::extend_text(text, delta, limits)?;
                    update(current, provider_metadata.as_ref());
                }
                self.write();
            }
            UiChunk::ReasoningEnd {
                id,
                provider_metadata,
            } => {
                let index = self.active_index(Active::Reasoning, chunk, id)?;
                if let Some(Part::Reasoning {
                    state,
                    provider_metadata: current,
                    ..
                }) = self.parts.get_mut(index)
                {
                    *state = PartState::Done;
                    update(current, provider_metadata.as_ref());
                }
                self.active_reasoning.remove(id);
                self.write();
            }
            UiChunk::File {
                url,
                media_type,
                provider_metadata,
            } => self.push(
                Part::File {
                    media_type: media_type.clone(),
                    url: url.clone(),
                    provider_metadata: provider_metadata.clone(),
                },
                limits,
            )?,
            UiChunk::ReasoningFile {
                url,
                media_type,
                provider_metadata,
            } => self.push(
                Part::ReasoningFile {
                    media_type: media_type.clone(),
                    url: url.clone(),
                    provider_metadata: provider_metadata.clone(),
                },
                limits,
            )?,
            UiChunk::SourceUrl {
                source_id,
                url,
                title,
                provider_metadata,
            } => self.push(
                Part::SourceUrl {
                    source_id: source_id.clone(),
                    url: url.clone(),
                    title: title.clone(),
                    provider_metadata: provider_metadata.clone(),
                },
                limits,
            )?,
            UiChunk::SourceDocument {
                source_id,
                media_type,
                title,
                filename,
                provider_metadata,
            } => self.push(
                Part::SourceDocument {
                    source_id: source_id.clone(),
                    media_type: media_type.clone(),
                    title: title.clone(),
                    filename: filename.clone(),
                    provider_metadata: provider_metadata.clone(),
                },
                limits,
            )?,
            UiChunk::Custom {
                kind,
                provider_metadata,
            } => self.push(
                Part::Custom {
                    kind: kind.clone(),
                    provider_metadata: provider_metadata.clone(),
                },
                limits,
            )?,
            UiChunk::ToolInputStart {
                tool_call_id,
                tool_name,
                title,
                provider_executed,
                provider_metadata,
                tool_metadata,
                dynamic,
            } => {
                let kind = ToolKind::of(*dynamic);
                let mut change = ToolUpdate::new(
                    kind,
                    tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::InputStreaming,
                );
                change.provider_executed = *provider_executed;
                change.provider_metadata.clone_from(provider_metadata);
                change.title.clone_from(title);
                change.tool_metadata.clone_from(tool_metadata);
                self.update_tool(change, None, limits)?;
                self.partial_tool_calls.insert(
                    tool_call_id.clone(),
                    PartialToolCall {
                        text: String::new(),
                        kind,
                        tool_name: tool_name.clone(),
                        title: title.clone(),
                        tool_metadata: tool_metadata.clone(),
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
                let mut change = ToolUpdate::new(
                    partial.kind,
                    partial.tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::InputStreaming,
                );
                change.input = parse_partial_json(&text);
                change.raw_input = Some(Value::String(text.clone()));
                change.title.clone_from(&partial.title);
                change.tool_metadata.clone_from(&partial.tool_metadata);
                self.update_tool(change, None, limits)?;
                if let Some(partial) = self.partial_tool_calls.get_mut(tool_call_id) {
                    partial.text = text;
                }
                self.write();
            }
            UiChunk::ToolInputAvailable {
                tool_call_id,
                tool_name,
                input,
                title,
                provider_executed,
                provider_metadata,
                tool_metadata,
                dynamic,
            } => {
                let mut change = ToolUpdate::new(
                    ToolKind::of(*dynamic),
                    tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::InputAvailable,
                );
                change.input = Some(input.clone());
                change.provider_executed = *provider_executed;
                change.provider_metadata.clone_from(provider_metadata);
                change.title.clone_from(title);
                change.tool_metadata.clone_from(tool_metadata);
                self.update_tool(change, None, limits)?;
                self.write();
            }
            UiChunk::ToolInputError {
                tool_call_id,
                tool_name,
                input,
                error_text,
                title: _,
                provider_executed,
                provider_metadata,
                tool_metadata,
                dynamic,
            } => {
                // An existing part of the current step keeps its kind.
                let kind = self
                    .tool_in_step(tool_call_id, Lookup::Any)
                    .and_then(|index| self.tool(index))
                    .map_or_else(|| ToolKind::of(*dynamic), |tool| tool.kind);
                let mut change = ToolUpdate::new(
                    kind,
                    tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::OutputError,
                );
                change.input = Some(input.clone());
                change.error_text = Some(error_text.clone());
                change.provider_executed = *provider_executed;
                change.provider_metadata.clone_from(provider_metadata);
                change.tool_metadata.clone_from(tool_metadata);
                self.update_tool(change, None, limits)?;
                self.write();
            }
            UiChunk::ToolApprovalRequest {
                approval_id,
                tool_call_id,
                approval_descriptor,
                input_schema_input,
                reason,
                is_automatic,
                signature,
            } => {
                let index = self
                    .tool_invocation(tool_call_id)
                    .ok_or_else(|| Self::missing_tool(chunk, tool_call_id))?;
                if let Some(tool) = self.tool_mut(index) {
                    tool.state = ToolState::ApprovalRequested;
                    tool.approval = Some(Approval {
                        id: approval_id.clone(),
                        approved: None,
                        descriptor: approval_descriptor
                            .clone()
                            .filter(|descriptor| !descriptor.is_null()),
                        request_reason: reason.clone(),
                        reason: None,
                        is_automatic: is_automatic.filter(|automatic| *automatic),
                        signature: signature.clone(),
                        input_schema_input: input_schema_input.clone(),
                    });
                }
                self.write();
            }
            UiChunk::ToolApprovalResponse {
                approval_id,
                approved,
                reason,
                provider_executed,
                provider_metadata,
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
                        if reason.is_some() {
                            approval.reason.clone_from(reason);
                        }
                    }
                    if provider_executed.is_some() {
                        tool.provider_executed = *provider_executed;
                    }
                    if provider_metadata.is_some() {
                        tool.call_provider_metadata.clone_from(provider_metadata);
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
                preliminary,
                provider_executed,
                provider_metadata,
                tool_metadata,
                dynamic: _,
            } => {
                let (index, tool) = self.existing_tool(chunk, tool_call_id)?;
                let mut change = ToolUpdate::new(
                    tool.kind,
                    tool.tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::OutputAvailable,
                );
                change.input.clone_from(&tool.input);
                change.output = Some(output.clone());
                change.preliminary = *preliminary;
                change.provider_executed = *provider_executed;
                change.provider_metadata.clone_from(provider_metadata);
                change.title.clone_from(&tool.title);
                change.tool_metadata = tool_metadata.clone().or_else(|| tool.tool_metadata.clone());
                self.update_tool(change, Some(index), limits)?;
                self.write();
            }
            UiChunk::ToolOutputError {
                tool_call_id,
                error_text,
                provider_executed,
                provider_metadata,
                tool_metadata,
                dynamic: _,
            } => {
                let (index, tool) = self.existing_tool(chunk, tool_call_id)?;
                let mut change = ToolUpdate::new(
                    tool.kind,
                    tool.tool_name.clone(),
                    tool_call_id.clone(),
                    ToolState::OutputError,
                );
                change.input.clone_from(&tool.input);
                // The AI SDK keeps a static part's raw input on error, not a dynamic one's.
                change.raw_input = match tool.kind {
                    ToolKind::Static => tool.raw_input.clone(),
                    ToolKind::Dynamic => None,
                };
                change.error_text = Some(error_text.clone());
                change.provider_executed = *provider_executed;
                change.provider_metadata.clone_from(provider_metadata);
                change.title.clone_from(&tool.title);
                change.tool_metadata = tool_metadata.clone().or_else(|| tool.tool_metadata.clone());
                self.update_tool(change, Some(index), limits)?;
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
                self.active_reasoning.clear();
                self.partial_tool_calls.clear();
                let start = self.step_start();
                if start < self.parts.len() {
                    self.parts.truncate(start);
                    self.write();
                }
            }
            UiChunk::Start {
                message_id,
                message_metadata,
            } => {
                if let Some(id) = message_id {
                    id.clone_into(&mut self.id);
                }
                let metadata_changed = self.update_metadata(message_metadata.as_ref());
                if message_id.is_some() || metadata_changed {
                    self.write();
                }
            }
            UiChunk::Finish {
                message_metadata, ..
            } => self.write_metadata(message_metadata.as_ref()),
            UiChunk::MessageMetadata { message_metadata } => {
                self.write_metadata(Some(message_metadata));
            }
            // Steps end and messages abort without changing the message;
            // errors and session data are the turn's (see `TranscriptTurn`).
            UiChunk::FinishStep
            | UiChunk::Abort { .. }
            | UiChunk::Error { .. }
            | UiChunk::UserMessage { .. }
            | UiChunk::TurnStarted { .. }
            | UiChunk::TurnEnded { .. }
            | UiChunk::Unknown => {}
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

    fn active_index(
        &self,
        active: Active,
        chunk: &UiChunk,
        id: &PartId,
    ) -> Result<usize, FoldError> {
        let map = match active {
            Active::Text => &self.active_text,
            Active::Reasoning => &self.active_reasoning,
        };
        map.get(id).copied().ok_or_else(|| FoldError::NoActivePart {
            chunk: chunk.kind(),
            id: id.clone(),
        })
    }
}

/// Which streaming parts a delta or end chunk refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Active {
    Text,
    Reasoning,
}

/// One user message and everything the stream says about the reply to it.
#[derive(Debug, Clone)]
pub struct TranscriptTurn {
    id: TurnId,
    text: String,
    message_id: Option<MessageId>,
    model: Option<String>,
    errors: Vec<String>,
    end: Option<TurnEndReason>,
    revision: u64,
    draft: ReplyBuilder,
}

impl TranscriptTurn {
    const fn new(id: TurnId, text: String, message_id: Option<MessageId>) -> Self {
        Self {
            id,
            text,
            message_id,
            model: None,
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

    /// What the user typed.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The client-minted id of the message that started the turn, when the
    /// stream carries it.
    #[must_use]
    pub const fn message_id(&self) -> Option<MessageId> {
        self.message_id
    }

    /// The model the turn runs on, once it has started.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
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

    fn apply(&mut self, chunk: &UiChunk, limits: FoldLimits) -> Result<(), FoldError> {
        if let UiChunk::TurnStarted { id, payload } = chunk {
            self.check_turn(chunk, *id)?;
            self.model = Some(payload.model.clone());
        } else if let UiChunk::TurnEnded { id, payload } = chunk {
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
        self.revision = self
            .revision
            .checked_add(1)
            .expect("a turn sees fewer than u64::MAX chunks");
        Ok(())
    }
}

impl Serialize for TranscriptTurn {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'turn> {
            id: &'turn TurnId,
            text: &'turn str,
            reply: Option<Reply<'turn>>,
            errors: &'turn [String],
            end: Option<TurnEndReason>,
        }
        View {
            id: &self.id,
            text: &self.text,
            reply: self.reply(),
            errors: &self.errors,
            end: self.end,
        }
        .serialize(serializer)
    }
}

/// The conversation so far: a turn per user message, in order.
#[derive(Debug, Clone)]
pub struct Transcript {
    turns: Vec<TranscriptTurn>,
    limits: FoldLimits,
}

impl Transcript {
    #[must_use]
    pub fn new(limits: &WireLimits) -> Self {
        Self {
            turns: Vec::with_capacity(limits.max_turns),
            limits: FoldLimits {
                turns: limits.max_turns,
                parts: limits.max_parts_per_reply,
                part_bytes: limits.max_part_bytes,
                errors: limits.max_errors_per_turn,
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

    /// Folds one chunk in. A [`UiChunk::Unknown`] is skipped.
    ///
    /// # Errors
    /// A chunk that does not fit the transcript (see [`FoldError`]). The
    /// transcript is unchanged; the stream it came from is broken.
    pub fn apply(&mut self, chunk: &UiChunk) -> Result<(), FoldError> {
        if let UiChunk::UserMessage { id, payload } = chunk {
            if self.turns.len() >= self.limits.turns {
                return Err(FoldError::TooManyTurns {
                    limit: self.limits.turns,
                });
            }
            self.turns.push(TranscriptTurn::new(
                *id,
                payload.text.clone(),
                payload.message_id,
            ));
            return Ok(());
        }
        if matches!(chunk, UiChunk::Unknown) {
            return Ok(());
        }
        let limits = self.limits;
        let turn = self.turns.last_mut().ok_or(FoldError::BeforeFirstTurn {
            chunk: chunk.kind(),
        })?;
        turn.apply(chunk, limits)
    }
}

/// `chunk.providerMetadata ?? part.providerMetadata`.
fn update(current: &mut Option<ProviderMetadata>, next: Option<&ProviderMetadata>) {
    if let Some(next) = next {
        *current = Some(next.clone());
    }
}

/// The AI SDK's `mergeObjects`: objects merge recursively, anything else is replaced.
fn merge(base: Value, overrides: &Value) -> Value {
    match (base, overrides) {
        (Value::Object(mut base), Value::Object(overrides)) => {
            for (key, value) in overrides {
                let merged = match base.remove(key) {
                    Some(existing) => merge(existing, value),
                    None => value.clone(),
                };
                base.insert(key.clone(), merged);
            }
            Value::Object(base)
        }
        (_, overrides) => overrides.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn metadata_merges_like_the_ai_sdk() {
        let merged = merge(
            json!({"a": {"b": "1", "c": "2"}, "d": ["1"]}),
            &json!({"a": {"c": "3"}, "d": {"x": null}, "e": "4"}),
        );
        assert_eq!(
            merged,
            json!({"a": {"b": "1", "c": "3"}, "d": {"x": null}, "e": "4"})
        );
    }
}
