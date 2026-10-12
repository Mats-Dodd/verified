//! AI SDK UI message stream chunks (`x-vercel-ai-ui-message-stream: v1`).
//!
//! Ported from q's `server/src/domain/UiChunk.ts` and `StreamChunk.ts`, which
//! port `ai@7.0.127` `packages/ai/src/ui-message-stream/ui-message-chunks.ts`
//! (Copyright Vercel, Inc., Apache License 2.0). Field names belong to the
//! protocol, so the union is discriminated on `type` and fields are
//! camelCase. Unknown fields are ignored, like the original's loose objects.
//!
//! The open `data-${string}` chunk is replaced by a closed set of session data
//! chunks ([`UiChunk::UserMessage`], [`UiChunk::TurnStarted`],
//! [`UiChunk::TurnEnded`]) so every entry of a session stream is still a valid
//! UI message chunk. A chunk whose `type` this build does not know decodes to
//! [`UiChunk::Unknown`] and is skipped by the fold; a known type with
//! malformed fields is an error.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use verified_domain::id::{ApprovalId, CallId, MessageId, PartId, ToolName, TurnId};

/// A JSON object.
pub type JsonObject = serde_json::Map<String, Value>;

/// Provider-specific metadata, keyed by provider.
pub type ProviderMetadata = BTreeMap<String, JsonObject>;

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Error,
    Other,
}

/// How a turn ended, as clients see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
}

/// `data` of [`UiChunk::UserMessage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessageData {
    pub text: String,
    /// The client-minted id of the message that started the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
}

/// `data` of [`UiChunk::TurnStarted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedData {
    /// The model the turn runs on.
    pub model: String,
}

/// `data` of [`UiChunk::TurnEnded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndedData {
    pub reason: TurnEndReason,
}

/// Deserializes a field whose presence matters even when it is `null`
/// (the AI SDK checks `hasOwnProperty`): present `null` is `Some(Null)`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// One entry of a session stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum UiChunk {
    // Message lifecycle.
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
    },
    Finish {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
    },
    Abort {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    MessageMetadata {
        message_metadata: Value,
    },
    Error {
        error_text: String,
    },

    // Steps.
    StartStep,
    FinishStep,
    /// Removes every part added during the current step: the model call is
    /// being retried.
    ResetStep,

    // Text and reasoning.
    TextStart {
        id: PartId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    TextDelta {
        id: PartId,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    TextEnd {
        id: PartId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningStart {
        id: PartId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningDelta {
        id: PartId,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningEnd {
        id: PartId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },

    // Tools.
    ToolInputStart {
        tool_call_id: CallId,
        tool_name: ToolName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_metadata: Option<JsonObject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },
    ToolInputDelta {
        tool_call_id: CallId,
        input_text_delta: String,
    },
    ToolInputAvailable {
        tool_call_id: CallId,
        tool_name: ToolName,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_metadata: Option<JsonObject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },
    ToolInputError {
        tool_call_id: CallId,
        tool_name: ToolName,
        input: Value,
        error_text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_metadata: Option<JsonObject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },
    ToolApprovalRequest {
        approval_id: ApprovalId,
        tool_call_id: CallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_descriptor: Option<Value>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        input_schema_input: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_automatic: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolApprovalResponse {
        approval_id: ApprovalId,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolOutputAvailable {
        tool_call_id: CallId,
        output: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preliminary: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_metadata: Option<JsonObject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },
    ToolOutputError {
        tool_call_id: CallId,
        error_text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_metadata: Option<JsonObject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<bool>,
    },
    ToolOutputDenied {
        tool_call_id: CallId,
    },

    // Sources, files and provider-specific content.
    SourceUrl {
        source_id: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    SourceDocument {
        source_id: String,
        media_type: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    File {
        url: String,
        media_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningFile {
        url: String,
        media_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    /// `kind` is `<provider>.<kind>`.
    Custom {
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },

    // Session data chunks.
    /// A user message started a turn.
    #[serde(rename = "data-user-message")]
    UserMessage {
        id: TurnId,
        #[serde(rename = "data")]
        payload: UserMessageData,
    },
    /// The turn's first model call is starting.
    #[serde(rename = "data-turn-start")]
    TurnStarted {
        id: TurnId,
        #[serde(rename = "data")]
        payload: TurnStartedData,
    },
    /// The turn ended.
    #[serde(rename = "data-turn-end")]
    TurnEnded {
        id: TurnId,
        #[serde(rename = "data")]
        payload: TurnEndedData,
    },

    /// A chunk type this build does not know (a newer protocol). Skipped.
    /// Never produced by the daemon; serializes as `{"type":"unknown"}`.
    #[serde(other)]
    Unknown,
}

impl UiChunk {
    /// The chunk's `type`, for diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Finish { .. } => "finish",
            Self::Abort { .. } => "abort",
            Self::MessageMetadata { .. } => "message-metadata",
            Self::Error { .. } => "error",
            Self::StartStep => "start-step",
            Self::FinishStep => "finish-step",
            Self::ResetStep => "reset-step",
            Self::TextStart { .. } => "text-start",
            Self::TextDelta { .. } => "text-delta",
            Self::TextEnd { .. } => "text-end",
            Self::ReasoningStart { .. } => "reasoning-start",
            Self::ReasoningDelta { .. } => "reasoning-delta",
            Self::ReasoningEnd { .. } => "reasoning-end",
            Self::ToolInputStart { .. } => "tool-input-start",
            Self::ToolInputDelta { .. } => "tool-input-delta",
            Self::ToolInputAvailable { .. } => "tool-input-available",
            Self::ToolInputError { .. } => "tool-input-error",
            Self::ToolApprovalRequest { .. } => "tool-approval-request",
            Self::ToolApprovalResponse { .. } => "tool-approval-response",
            Self::ToolOutputAvailable { .. } => "tool-output-available",
            Self::ToolOutputError { .. } => "tool-output-error",
            Self::ToolOutputDenied { .. } => "tool-output-denied",
            Self::SourceUrl { .. } => "source-url",
            Self::SourceDocument { .. } => "source-document",
            Self::File { .. } => "file",
            Self::ReasoningFile { .. } => "reasoning-file",
            Self::Custom { .. } => "custom",
            Self::UserMessage { .. } => "data-user-message",
            Self::TurnStarted { .. } => "data-turn-start",
            Self::TurnEnded { .. } => "data-turn-end",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn decode(value: &Value) -> UiChunk {
        serde_json::from_value(value.clone()).expect("a chunk")
    }

    #[test]
    fn decodes_protocol_field_names() {
        assert_eq!(
            decode(&json!({"type":"text-delta","id":"t0","delta":"hi"})),
            UiChunk::TextDelta {
                id: PartId::parse("t0").expect("a part id"),
                delta: "hi".to_owned(),
                provider_metadata: None,
            }
        );
        let chunk = decode(&json!({
            "type":"tool-input-available","toolCallId":"toolu_1","toolName":"read",
            "input":{"path":"a.rs"}
        }));
        assert_eq!(chunk.kind(), "tool-input-available");
        assert_eq!(
            serde_json::to_value(&chunk).expect("serializes"),
            json!({"type":"tool-input-available","toolCallId":"toolu_1","toolName":"read","input":{"path":"a.rs"}})
        );
    }

    #[test]
    fn unknown_types_decode_to_unknown_and_unit_chunks_ignore_extra_fields() {
        assert_eq!(
            decode(&json!({"type":"from-the-future","x":"y"})),
            UiChunk::Unknown
        );
        assert_eq!(
            decode(&json!({"type":"data-something-else","data":{}})),
            UiChunk::Unknown
        );
        assert_eq!(
            decode(&json!({"type":"reset-step","extra":true})),
            UiChunk::ResetStep
        );
        assert_eq!(
            serde_json::to_value(UiChunk::StartStep).expect("serializes"),
            json!({"type":"start-step"})
        );
    }

    #[test]
    fn malformed_known_chunks_are_errors() {
        assert_eq!(
            serde_json::from_value::<UiChunk>(json!({"type":"text-delta","id":"t0"})).ok(),
            None
        );
        assert_eq!(
            serde_json::from_value::<UiChunk>(
                json!({"type":"data-turn-end","id":"not-a-uuid","data":{"reason":"failed"}})
            )
            .ok(),
            None
        );
        assert_eq!(
            serde_json::from_value::<UiChunk>(json!({"kind":"start"})).ok(),
            None
        );
    }

    #[test]
    fn input_schema_input_keeps_a_present_null() {
        let chunk = decode(&json!({
            "type":"tool-approval-request","approvalId":"a","toolCallId":"c","inputSchemaInput":null
        }));
        assert!(matches!(
            chunk,
            UiChunk::ToolApprovalRequest {
                input_schema_input: Some(Value::Null),
                ..
            }
        ));
    }
}
