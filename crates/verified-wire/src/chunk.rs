//! AI SDK UI message stream chunks (`x-vercel-ai-ui-message-stream: v1`).
//!
//! Ported from q's `server/src/domain/UiChunk.ts` and `StreamChunk.ts`, which
//! port `ai@7.0.127` `packages/ai/src/ui-message-stream/ui-message-chunks.ts`
//! (Copyright Vercel, Inc., Apache License 2.0), cut to the kinds and fields
//! the daemon emits. Field names belong to the protocol, so the union is
//! discriminated on `type` and fields are camelCase. Unknown fields are
//! ignored, like the original's loose objects.
//!
//! The open `data-${string}` chunk is replaced by a closed set of session data
//! chunks ([`UiChunk::UserMessage`], [`UiChunk::TurnStarted`],
//! [`UiChunk::TurnEnded`]) so every entry of a session stream is still a valid
//! UI message chunk. The daemon and the TUI ship in one binary, so a `type`
//! this build does not know, like a known type with malformed fields, is an
//! error.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use verified_domain::id::{ApprovalId, CallId, MessageId, PartId, ToolName, TurnId};

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
    },
    Finish {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
    },
    Abort {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
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

    // Text.
    TextStart {
        id: PartId,
    },
    TextDelta {
        id: PartId,
        delta: String,
    },
    TextEnd {
        id: PartId,
    },

    // Tools.
    ToolInputStart {
        tool_call_id: CallId,
        tool_name: ToolName,
    },
    ToolInputDelta {
        tool_call_id: CallId,
        input_text_delta: String,
    },
    ToolInputAvailable {
        tool_call_id: CallId,
        tool_name: ToolName,
        input: Value,
    },
    ToolApprovalRequest {
        approval_id: ApprovalId,
        tool_call_id: CallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_automatic: Option<bool>,
    },
    ToolApprovalResponse {
        approval_id: ApprovalId,
        approved: bool,
    },
    ToolOutputAvailable {
        tool_call_id: CallId,
        output: Value,
    },
    ToolOutputError {
        tool_call_id: CallId,
        error_text: String,
    },
    ToolOutputDenied {
        tool_call_id: CallId,
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
}

impl UiChunk {
    /// The chunk's `type`, for diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Finish { .. } => "finish",
            Self::Abort { .. } => "abort",
            Self::Error { .. } => "error",
            Self::StartStep => "start-step",
            Self::FinishStep => "finish-step",
            Self::ResetStep => "reset-step",
            Self::TextStart { .. } => "text-start",
            Self::TextDelta { .. } => "text-delta",
            Self::TextEnd { .. } => "text-end",
            Self::ToolInputStart { .. } => "tool-input-start",
            Self::ToolInputDelta { .. } => "tool-input-delta",
            Self::ToolInputAvailable { .. } => "tool-input-available",
            Self::ToolApprovalRequest { .. } => "tool-approval-request",
            Self::ToolApprovalResponse { .. } => "tool-approval-response",
            Self::ToolOutputAvailable { .. } => "tool-output-available",
            Self::ToolOutputError { .. } => "tool-output-error",
            Self::ToolOutputDenied { .. } => "tool-output-denied",
            Self::UserMessage { .. } => "data-user-message",
            Self::TurnStarted { .. } => "data-turn-start",
            Self::TurnEnded { .. } => "data-turn-end",
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
    fn unit_chunks_ignore_extra_fields() {
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
    fn unknown_and_malformed_chunks_are_errors() {
        for value in [
            json!({"type":"from-the-future","x":"y"}),
            json!({"type":"reasoning-start","id":"r0"}),
            json!({"type":"data-something-else","data":{}}),
            json!({"type":"text-delta","id":"t0"}),
            json!({"type":"data-turn-end","id":"not-a-uuid","data":{"reason":"failed"}}),
            json!({"kind":"start"}),
        ] {
            assert_eq!(
                serde_json::from_value::<UiChunk>(value.clone()).ok(),
                None,
                "{value}"
            );
        }
    }
}
