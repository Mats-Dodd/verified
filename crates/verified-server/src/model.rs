//! The Model seam: stream a response to a request as [`ModelPart`]s.
//!
//! A model reports typed failures (`Overloaded`, `Network`, `Other`,
//! `Interrupted`); retrying is the core's job, never the seam's. Cancelling
//! is cooperative: the stream checks its token between reads and stops
//! reading, which discards the rest of the response.

pub mod anthropic;
pub mod fake;

use core::fmt;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::id::{CallId, ToolName};
use verified_domain::{Cause, Fault};

use crate::payload::ModelPart;

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Whether a tool result reports an error to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    Success,
    Error,
}

/// One block of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text {
        text: String,
    },
    ToolUse {
        id: CallId,
        name: ToolName,
        input: Value,
    },
    ToolResult {
        id: CallId,
        text: String,
        kind: ResultKind,
    },
}

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

/// What the model is asked: the conversation so far, rebuilt from the log.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
}

/// Why a model call did not finish.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// The provider is overloaded or rate limiting: retryable.
    #[error("the model is overloaded: {0}")]
    Overloaded(String),
    /// The connection failed or the stream broke off: retryable.
    #[error("the network failed: {0}")]
    Network(String),
    /// Anything else (a bad request, an unparseable stream): not retried.
    #[error("the model call failed: {0}")]
    Other(String),
    /// The call's cancel token fired; the stream was discarded.
    #[error("the model call was interrupted")]
    Interrupted,
}

impl ModelError {
    /// The core's view of this failure.
    #[must_use]
    pub const fn core_cause(&self) -> Cause {
        match self {
            Self::Overloaded(_) => Cause::Failed(Fault::Overloaded),
            Self::Network(_) => Cause::Failed(Fault::Network),
            Self::Other(_) => Cause::Failed(Fault::Other),
            Self::Interrupted => Cause::Interrupted,
        }
    }
}

/// The Model seam.
#[async_trait]
pub trait Model: Send + Sync + fmt::Debug {
    /// Streams the response to `request` into `parts`, in order, and returns
    /// once the response is complete. Dropping `parts` (by returning) tells
    /// the receiver the stream is over.
    ///
    /// # Errors
    /// A typed failure; parts already sent stay sent.
    async fn stream(
        &self,
        request: &ModelRequest,
        parts: mpsc::Sender<ModelPart>,
        cancel: &CancellationToken,
    ) -> Result<(), ModelError>;
}

/// The tools the model is offered, as Anthropic tool definitions.
#[must_use]
pub fn tool_definitions() -> Value {
    serde_json::json!([
        {
            "name": "read",
            "description": "Read a UTF-8 file in the project, or list a directory. Paths are relative to the project root.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Relative path, no `..`."}},
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "edit",
            "description": "Replace the single occurrence of `old` with `new` in a project file. Include enough context in `old` to make it unique. Asks the user first.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"}
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }
        },
        {
            "name": "bash",
            "description": "Run a shell command in the project root and return its combined output and exit status. Asks the user first; runs alone.",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }
        }
    ])
}
