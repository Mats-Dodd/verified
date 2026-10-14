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
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::id::{CallId, ToolName};
use verified_domain::{ModelFault, Tool};

use crate::payload::{ModelPart, tool_name};

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
    /// The tools the actor is offered (its `Born`), in this order.
    pub tools: Vec<Tool>,
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
    pub const fn core_fault(&self) -> ModelFault {
        match self {
            Self::Overloaded(_) => ModelFault::Overloaded,
            Self::Network(_) => ModelFault::Network,
            Self::Other(_) => ModelFault::Other,
            Self::Interrupted => ModelFault::Cancelled,
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
pub fn tool_definitions(tools: &[Tool]) -> Value {
    Value::Array(tools.iter().map(|tool| tool_definition(*tool)).collect())
}

/// One tool's name, description and input schema.
fn tool_definition(tool: Tool) -> Value {
    let (description, properties, required) = match tool {
        Tool::Read => (
            "Read a UTF-8 file in the project, or list a directory. Paths are relative to the project root.",
            json!({"path": {"type": "string", "description": "Relative path, no `..`."}}),
            json!(["path"]),
        ),
        Tool::Edit => (
            "Replace the single occurrence of `old` with `new` in a project file. Include enough context in `old` to make it unique.",
            json!({"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}}),
            json!(["path", "old", "new"]),
        ),
        Tool::Bash => (
            "Run a shell command in the project root and return its combined output and exit status. With `background`, it runs on as a task: the call returns its task id at once, and you are told when it ends.",
            json!({"command": {"type": "string"}, "background": {"type": "boolean"}}),
            json!(["command"]),
        ),
        Tool::Spawn => (
            "Start a new agent named `name`, offered `tools`, and send it `prompt`. Returns its address. It runs on its own; it messages you with `send` if it wants to.",
            json!({
                "name": {"type": "string", "description": "1 to 64 of [A-Za-z0-9_-]."},
                "tools": {"type": "array", "items": {"type": "string", "enum": ["read", "edit", "bash", "spawn", "send", "wake", "tasks", "cancel"]}},
                "prompt": {"type": "string"}
            }),
            json!(["name", "tools", "prompt"]),
        ),
        Tool::Send => (
            "Send `text` to an address: `user`, or an agent's address. It lands in their mailbox; nothing waits for an answer.",
            json!({
                "to": {"type": "string"},
                "text": {"type": "string"},
                "reply_to": {"type": "string", "description": "Where answers should go; metadata only."}
            }),
            json!(["to", "text"]),
        ),
        Tool::Wake => (
            "Start a timer task: you are told when `after_ms` milliseconds have passed.",
            json!({"after_ms": {"type": "integer", "minimum": 0u64}}),
            json!(["after_ms"]),
        ),
        Tool::Tasks => (
            "List your tasks: background commands, timers and stops, running or ended.",
            json!({}),
            json!([]),
        ),
        Tool::Cancel => (
            "Cancel one of your tasks by its id.",
            json!({"task": {"type": "integer", "minimum": 0u64}}),
            json!(["task"]),
        ),
    };
    json!({
        "name": tool_name(tool),
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
    })
}
