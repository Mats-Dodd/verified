//! What the daemon's core carries: `Core<Parsed>`.
//!
//! Every payload is parsed exactly once at its boundary (the user's text from
//! the wire, model parts from the Anthropic stream, tool arguments from the
//! model's JSON, a log line on replay) and the rest of the daemon only sees
//! these types. Each record slot has its own type, so a record whose payload
//! is in the wrong slot does not deserialize.

use core::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use verified_domain::id::{ActorId, CallId, TaskId, ToolName, UnixMillis};
use verified_domain::{
    ActorName, Address, Measured, Millis, Payload, Proposal, Request, Shape, Tool,
};
use verified_wire::chunk::FinishReason;

use crate::seam::{Clock, Entropy, id_entropy};

/// The daemon's payloads: each slot holds the type its boundary parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Parsed;

impl Payload for Parsed {
    /// What the user typed (bounded by `MessageText` when it arrived).
    type Text = String;
    /// A batch of model stream parts, exactly as streamed.
    type Parts = Vec<ModelPart>;
    type Args = ToolArgs;
    /// Why a proposed call is invalid; the model is told.
    type Reason = String;
    type Output = ToolOutput;

    fn shape(args: &ToolArgs) -> Shape {
        args.shape()
    }

    fn tool(args: &ToolArgs) -> Tool {
        args.tool()
    }
}

/// `body` with its size, counted as its JSON length: what
/// `Limits::context_bytes` bounds.
///
/// # Panics
/// Never: payloads have string keys and serialize infallibly.
#[must_use]
pub fn measure<T: Serialize>(body: T) -> Measured<T> {
    let bytes = serde_json::to_vec(&body)
        .map(|json| json.len())
        .expect("payloads have string keys and serialize infallibly");
    Measured {
        body,
        bytes: u64::try_from(bytes).expect("a payload is smaller than 2^64 bytes"),
    }
}

/// One part of a model's streamed response, in the AI SDK's stream-part
/// vocabulary. Text parts are named by the response's content block index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ModelPart {
    TextStart {
        block: u32,
    },
    TextDelta {
        block: u32,
        delta: String,
    },
    TextEnd {
        block: u32,
    },
    ToolInputStart {
        id: CallId,
        tool_name: ToolName,
    },
    ToolInputDelta {
        id: CallId,
        delta: String,
    },
    ToolInputEnd {
        id: CallId,
    },
    /// A complete tool call: `input` is the JSON object the model sent,
    /// parsed when the call completed.
    ToolCall {
        tool_call_id: CallId,
        tool_name: ToolName,
        input: ToolInput,
    },
    Finish {
        finish_reason: FinishReason,
    },
}

/// A tool call's input as the model sent it: a JSON object. Input that is
/// not one fails the model response at the boundary; nothing downstream
/// substitutes anything for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolInput(Map<String, Value>);

impl ToolInput {
    /// The input of a call with streamed JSON `text` (empty: no input).
    ///
    /// # Errors
    /// `text` is not a JSON object.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Ok(Self::default());
        }
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => Ok(Self(object)),
            Ok(other) => Err(format!("tool input is not a JSON object: {other}")),
            Err(error) => Err(format!("tool input is not JSON: {error}")),
        }
    }

    /// The input as a JSON value (always an object).
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }
}

/// A path inside the project: relative, normalized, with no `..`, valid
/// UTF-8, and outside the daemon's own state. The only kind of path the
/// tools accept.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectPath(String);

/// Why text is not a [`ProjectPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("the path is empty")]
    Empty,
    #[error("the path is longer than {limit} bytes")]
    TooLong { limit: usize },
    #[error("the path must be relative to the project")]
    Absolute,
    #[error("the path must not contain `..`, `.` or empty segments")]
    NotNormal,
    #[error("the path contains a NUL byte")]
    Nul,
    /// A component names the daemon's state directory, or has the shape of
    /// an edit's staged file.
    #[error("the path is reserved for the agent's own state")]
    Reserved,
}

/// What follows a file's name in the name of the file an `edit` writes
/// before renaming it over the file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sibling(String);

impl Sibling {
    /// The suffix of the staged file of the edit dispatched by record `seq`
    /// of `session`: `verified-<session>-<seq>`, unique to that call. A
    /// session id's text is lowercase hex and `-`, a seq is decimal digits.
    #[must_use]
    pub fn staged(session: ActorId, seq: u64) -> Self {
        Self(format!("verified-{session}-{seq}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The name of the daemon's state directory, which no tool may touch.
pub const STATE_DIR: &str = crate::daemon::DIR;

/// What every edit's staged file has in its name before its key: a
/// component containing it is reserved.
pub const STAGED_MARKER: &str = ".verified-";

/// Whether `component` is the state directory or looks like a staged file,
/// ignoring ASCII case (the default macOS filesystem does too).
fn reserved(component: &str) -> bool {
    component.eq_ignore_ascii_case(STATE_DIR)
        || component.to_ascii_lowercase().contains(STAGED_MARKER)
}

impl ProjectPath {
    /// The longest path accepted, in bytes.
    pub const MAX_BYTES: usize = 3840;

    /// # Errors
    /// Empty, too long, absolute, not normalized, containing NUL, or naming
    /// the agent's own state.
    pub fn parse(text: &str) -> Result<Self, PathError> {
        if text.is_empty() {
            return Err(PathError::Empty);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(PathError::TooLong {
                limit: Self::MAX_BYTES,
            });
        }
        if text.contains('\0') {
            return Err(PathError::Nul);
        }
        if text.starts_with('/') {
            return Err(PathError::Absolute);
        }
        let mut normal = true;
        for segment in text.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                normal = false;
            }
        }
        for component in Path::new(text).components() {
            match component {
                Component::Normal(_) => {}
                Component::Prefix(_)
                | Component::RootDir
                | Component::CurDir
                | Component::ParentDir => normal = false,
            }
        }
        if !normal {
            return Err(PathError::NotNormal);
        }
        if text.split('/').any(reserved) {
            return Err(PathError::Reserved);
        }
        Ok(Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path under `root`.
    #[must_use]
    pub fn under(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }
}

impl TryFrom<String> for ProjectPath {
    type Error = PathError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ProjectPath> for String {
    fn from(path: ProjectPath) -> Self {
        path.0
    }
}

impl fmt::Display for ProjectPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A shell command for `bash`: non-empty and bounded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Command(String);

impl Command {
    /// The longest command accepted, in bytes.
    pub const MAX_BYTES: usize = 64 << 10;

    /// # Errors
    /// Empty or longer than [`Command::MAX_BYTES`].
    pub fn parse(text: String) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Err("the command is empty".to_owned());
        }
        if text.len() > Self::MAX_BYTES {
            return Err(format!(
                "the command is longer than {} bytes",
                Self::MAX_BYTES
            ));
        }
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Command {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(text)
    }
}

impl From<Command> for String {
    fn from(command: Command) -> Self {
        command.0
    }
}

/// The longest a `wake` may wait: a day.
pub const MAX_WAKE: Millis = Millis::new(24 * 60 * 60 * 1000);

/// What the model boundary knows when it parses a call.
///
/// The tools this actor is offered, the bound on an envelope's text, and
/// the seams that mint what a call needs before its `StepFinished` records
/// it (a spawned child's id, a timer's deadline), so a rerun after a crash
/// reuses them.
#[derive(Clone, Copy)]
pub struct Boundary<'seams> {
    pub tools: &'seams [Tool],
    pub text_bytes: usize,
    pub clock: &'seams dyn Clock,
    pub entropy: &'seams dyn Entropy,
}

impl fmt::Debug for Boundary<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Boundary")
            .field("tools", &self.tools)
            .field("text_bytes", &self.text_bytes)
            .finish_non_exhaustive()
    }
}

/// A tool's name, as the model and the rules file spell it.
#[must_use]
pub const fn tool_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Read => "read",
        Tool::Edit => "edit",
        Tool::Bash => "bash",
        Tool::Spawn => "spawn",
        Tool::Send => "send",
        Tool::Wake => "wake",
        Tool::Tasks => "tasks",
        Tool::Cancel => "cancel",
    }
}

/// The tool spelled `name`, if any.
#[must_use]
pub fn tool_named(name: &str) -> Option<Tool> {
    [
        Tool::Read,
        Tool::Edit,
        Tool::Bash,
        Tool::Spawn,
        Tool::Send,
        Tool::Wake,
        Tool::Tasks,
        Tool::Cancel,
    ]
    .into_iter()
    .find(|tool| tool_name(*tool) == name)
}

/// A tool call's arguments, parsed from the model's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ToolArgs {
    /// Read a file (or list a directory).
    Read { path: ProjectPath },
    /// Replace the one occurrence of `old` with `new`.
    Edit {
        path: ProjectPath,
        old: String,
        new: String,
    },
    /// Run a shell command in the project; in the background it runs on as
    /// a task.
    Bash {
        command: Command,
        #[serde(default, skip_serializing_if = "core::ops::Not::not")]
        background: bool,
    },
    /// Make a child actor named `name`, offered `tools`, and deliver it
    /// `prompt`. `child` is minted at the boundary.
    Spawn {
        name: ActorName,
        tools: Vec<Tool>,
        prompt: String,
        child: ActorId,
    },
    /// Deliver `text` to `to`'s mailbox.
    Send {
        to: Address,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<Address>,
    },
    /// A timer task that ends `after` from when it was proposed, at `at`
    /// (minted at the boundary).
    Wake {
        after: Millis,
        #[serde(with = "unix_millis")]
        at: UnixMillis,
    },
    /// This actor's tasks.
    Tasks,
    /// Cancel one of this actor's tasks.
    Cancel { task: TaskId },
}

impl ToolArgs {
    #[must_use]
    pub const fn tool(&self) -> Tool {
        match self {
            Self::Read { .. } => Tool::Read,
            Self::Edit { .. } => Tool::Edit,
            Self::Bash { .. } => Tool::Bash,
            Self::Spawn { .. } => Tool::Spawn,
            Self::Send { .. } => Tool::Send,
            Self::Wake { .. } => Tool::Wake,
            Self::Tasks => Tool::Tasks,
            Self::Cancel { .. } => Tool::Cancel,
        }
    }

    /// The call's shape: its tool and the subject a rule matches (the path
    /// of a `read` or an `edit`, the command of a `bash`, the recipient's
    /// address of a `send`, the child's name of a `spawn`).
    #[must_use]
    pub fn shape(&self) -> Shape {
        match self {
            Self::Read { path } => Shape::Read {
                subject: path.as_str().as_bytes().to_vec(),
            },
            Self::Edit { path, .. } => Shape::Edit {
                subject: path.as_str().as_bytes().to_vec(),
            },
            Self::Bash {
                command,
                background,
            } => Shape::Bash {
                subject: command.as_str().as_bytes().to_vec(),
                background: *background,
            },
            Self::Spawn { name, child, .. } => Shape::Spawn {
                subject: name.as_str().as_bytes().to_vec(),
                child: *child,
            },
            Self::Send { to, .. } => Shape::Send {
                subject: to.to_string().into_bytes(),
            },
            Self::Wake { .. } => Shape::Wake,
            Self::Tasks => Shape::Tasks,
            Self::Cancel { task } => Shape::Cancel { task: *task },
        }
    }

    /// Parses a tool call from the model. The error is the reason the model
    /// is told.
    ///
    /// # Errors
    /// An unknown tool or one this actor is not offered, input that is not
    /// the tool's JSON, or an invalid field.
    pub fn parse(name: &ToolName, input: &ToolInput, at: &Boundary<'_>) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ReadInput {
            path: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EditInput {
            path: String,
            old: String,
            new: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct BashInput {
            command: String,
            #[serde(default)]
            background: bool,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpawnInput {
            name: String,
            tools: Vec<String>,
            prompt: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SendInput {
            to: String,
            text: String,
            reply_to: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WakeInput {
            after_ms: u64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TasksInput {}
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CancelInput {
            task: u64,
        }

        let Some(tool) = tool_named(name.as_str()) else {
            return Err(format!("unknown tool `{name}`"));
        };
        if !at.tools.contains(&tool) {
            return Err(format!("this actor is not offered `{name}`"));
        }
        let bad = |error: serde_json::Error| format!("invalid input for `{name}`: {error}");
        let input = input.to_value();
        let path = |text: &str| {
            ProjectPath::parse(text).map_err(|error| format!("invalid path `{text}`: {error}"))
        };
        let text = |what: &str, text: String| {
            if text.trim().is_empty() {
                Err(format!("`{what}` is empty"))
            } else if text.len() > at.text_bytes {
                Err(format!("`{what}` is longer than {} bytes", at.text_bytes))
            } else {
                Ok(text)
            }
        };
        let address = |text: &str| {
            text.parse::<Address>()
                .map_err(|error| format!("invalid address `{text}`: {error}"))
        };
        match tool {
            Tool::Read => {
                let input: ReadInput = serde_json::from_value(input).map_err(bad)?;
                Ok(Self::Read {
                    path: path(&input.path)?,
                })
            }
            Tool::Edit => {
                let input: EditInput = serde_json::from_value(input).map_err(bad)?;
                if input.old.is_empty() {
                    return Err("`old` must not be empty".to_owned());
                }
                if input.old == input.new {
                    return Err("`old` and `new` are the same".to_owned());
                }
                Ok(Self::Edit {
                    path: path(&input.path)?,
                    old: input.old,
                    new: input.new,
                })
            }
            Tool::Bash => {
                let input: BashInput = serde_json::from_value(input).map_err(bad)?;
                Ok(Self::Bash {
                    command: Command::parse(input.command)?,
                    background: input.background,
                })
            }
            Tool::Spawn => {
                let input: SpawnInput = serde_json::from_value(input).map_err(bad)?;
                let mut tools = Vec::with_capacity(input.tools.len());
                for named in &input.tools {
                    let tool =
                        tool_named(named).ok_or_else(|| format!("unknown tool `{named}`"))?;
                    if !tools.contains(&tool) {
                        tools.push(tool);
                    }
                }
                Ok(Self::Spawn {
                    name: ActorName::parse(&input.name)
                        .map_err(|error| format!("invalid name `{}`: {error}", input.name))?,
                    tools,
                    prompt: text("prompt", input.prompt)?,
                    child: ActorId::from_parts(at.clock.now(), &id_entropy(at.entropy)),
                })
            }
            Tool::Send => {
                let input: SendInput = serde_json::from_value(input).map_err(bad)?;
                let to = address(&input.to)?;
                if to == Address::Runtime {
                    return Err("`v` takes no messages".to_owned());
                }
                Ok(Self::Send {
                    to,
                    text: text("text", input.text)?,
                    reply_to: input.reply_to.as_deref().map(address).transpose()?,
                })
            }
            Tool::Wake => {
                let input: WakeInput = serde_json::from_value(input).map_err(bad)?;
                let after = Millis::new(input.after_ms);
                if after > MAX_WAKE {
                    return Err(format!(
                        "`after_ms` is more than {} (a day)",
                        MAX_WAKE.get()
                    ));
                }
                let at = UnixMillis::new(at.clock.now().get().saturating_add(after.get()))
                    .map_err(|error| error.to_string())?;
                Ok(Self::Wake { after, at })
            }
            Tool::Tasks => {
                let _: TasksInput = serde_json::from_value(input).map_err(bad)?;
                Ok(Self::Tasks)
            }
            Tool::Cancel => {
                let input: CancelInput = serde_json::from_value(input).map_err(bad)?;
                Ok(Self::Cancel {
                    task: TaskId(input.task),
                })
            }
        }
    }
}

/// A [`UnixMillis`] in the log: its number.
mod unix_millis {
    use serde::{Deserialize as _, Deserializer, Serializer, de};
    use verified_domain::id::UnixMillis;

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` takes the field by reference"
    )]
    pub(super) fn serialize<S: Serializer>(
        at: &UnixMillis,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(at.get())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<UnixMillis, D::Error> {
        UnixMillis::new(u64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Whether output was cut to fit `ServerLimits::tool_output_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Truncation {
    Complete,
    Truncated { omitted_bytes: u64 },
}

/// What a tool returned to the model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolOutput {
    pub text: String,
    pub truncation: Truncation,
}

impl ToolOutput {
    #[must_use]
    pub const fn complete(text: String) -> Self {
        Self {
            text,
            truncation: Truncation::Complete,
        }
    }
}

/// The proposal a model's tool call becomes: `Run` when the tool and its
/// arguments parse, `Invalid` with the reason otherwise.
#[must_use]
pub fn proposal(
    id: &CallId,
    name: &ToolName,
    input: &ToolInput,
    at: &Boundary<'_>,
) -> Proposal<Parsed> {
    let request = match ToolArgs::parse(name, input, at) {
        Ok(args) => Request::Run { args },
        Err(reason) => Request::Invalid { reason },
    };
    Proposal {
        id: id.clone(),
        request,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Boundary, ModelPart, Parsed, PathError, ProjectPath, Sibling, ToolArgs, ToolInput, measure,
    };
    use crate::seam::{Clock, Entropy};
    use verified_domain::id::{ActorId, CallId, TaskId, ToolName, UnixMillis};
    use verified_domain::{ActorName, Address, Millis, Record, Shape, Tool};

    #[test]
    fn project_paths_stay_inside() {
        for good in ["a", "src/lib.rs", "a/b/c.txt", ".hidden"] {
            assert!(ProjectPath::parse(good).is_ok(), "{good}");
        }
        for (bad, error) in [
            ("", PathError::Empty),
            ("/etc/passwd", PathError::Absolute),
            ("../x", PathError::NotNormal),
            ("a/../b", PathError::NotNormal),
            ("a//b", PathError::NotNormal),
            ("./a", PathError::NotNormal),
            ("a/", PathError::NotNormal),
            ("a\0b", PathError::Nul),
            (".verified", PathError::Reserved),
            (".verified/sessions/x.log", PathError::Reserved),
            ("src/.VERIFIED/edits", PathError::Reserved),
            ("src/.lib.rs.verified-x-3", PathError::Reserved),
        ] {
            assert_eq!(ProjectPath::parse(bad), Err(error), "{bad:?}");
        }
        assert!(
            ProjectPath::parse("verified-x").is_ok(),
            "only the dotted marker is reserved"
        );
        assert_eq!(
            ProjectPath::parse(&"a".repeat(ProjectPath::MAX_BYTES + 1)),
            Err(PathError::TooLong {
                limit: ProjectPath::MAX_BYTES
            }),
            "too long"
        );
    }

    /// No tool may name the file an edit stages: its name holds the
    /// reserved marker.
    #[test]
    fn staged_names_are_reserved() {
        let session = ActorId::parse("01900000-0000-7000-8000-000000000001").expect("a v7");
        let staged = Sibling::staged(session, u64::MAX);
        let name = format!("src/.lib.rs.{}", staged.as_str());
        assert_eq!(ProjectPath::parse(&name), Err(PathError::Reserved));
    }

    /// A clock stopped at one instant, and entropy of one byte repeated.
    #[derive(Debug)]
    struct Fixed;

    #[async_trait::async_trait]
    impl Clock for Fixed {
        fn now(&self) -> UnixMillis {
            UnixMillis::new(1_700_000_000_000).expect("a time")
        }

        async fn sleep(&self, _delay: Millis) {}
    }

    impl Entropy for Fixed {
        fn fill(&self, out: &mut [u8]) {
            out.fill(9);
        }
    }

    const EVERY: [Tool; 8] = [
        Tool::Read,
        Tool::Edit,
        Tool::Bash,
        Tool::Spawn,
        Tool::Send,
        Tool::Wake,
        Tool::Tasks,
        Tool::Cancel,
    ];

    /// The boundary of an actor offered every tool.
    fn all() -> Boundary<'static> {
        Boundary {
            tools: &EVERY,
            text_bytes: 64,
            clock: &Fixed,
            entropy: &Fixed,
        }
    }

    /// The runtime's own tools parse at the boundary, which mints a
    /// spawned child's id and a timer's deadline, and refuses what this
    /// actor is not offered.
    #[test]
    fn runtime_tools_parse_and_mint_at_the_boundary() {
        let name = |text: &str| ToolName::parse(text).expect("a tool name");
        let input = |text: &str| ToolInput::parse(text).expect("a JSON object");
        let at = all();
        assert_eq!(
            ToolArgs::parse(&name("wake"), &input(r#"{"after_ms":250}"#), &at),
            Ok(ToolArgs::Wake {
                after: Millis::new(250),
                at: UnixMillis::new(1_700_000_000_250).expect("a time"),
            })
        );
        assert_eq!(
            ToolArgs::parse(
                &name("spawn"),
                &input(r#"{"name":"helper","tools":["read","send","read"],"prompt":"hi"}"#),
                &at
            ),
            Ok(ToolArgs::Spawn {
                name: ActorName::parse("helper").expect("a name"),
                tools: vec![Tool::Read, Tool::Send],
                prompt: "hi".to_owned(),
                child: ActorId::from_parts(Fixed.now(), &[9; 10]),
            })
        );
        assert_eq!(
            ToolArgs::parse(&name("send"), &input(r#"{"to":"user","text":"done"}"#), &at),
            Ok(ToolArgs::Send {
                to: Address::User,
                text: "done".to_owned(),
                reply_to: None,
            })
        );
        assert_eq!(
            ToolArgs::parse(
                &name("bash"),
                &input(r#"{"command":"make","background":true}"#),
                &at
            )
            .map(|args| args.shape()),
            Ok(Shape::Bash {
                subject: b"make".to_vec(),
                background: true
            })
        );
        assert_eq!(
            ToolArgs::parse(&name("cancel"), &input(r#"{"task":7}"#), &at),
            Ok(ToolArgs::Cancel { task: TaskId(7) })
        );
        assert_eq!(
            ToolArgs::parse(&name("tasks"), &input("{}"), &at),
            Ok(ToolArgs::Tasks)
        );
        for (tool, bad) in [
            ("send", r#"{"to":"v","text":"x"}"#),
            ("send", r#"{"to":"nobody","text":"x"}"#),
            ("send", r#"{"to":"user","text":"   "}"#),
            (
                "send",
                &format!(r#"{{"to":"user","text":"{}"}}"#, "x".repeat(65)),
            ),
            ("spawn", r#"{"name":"a b","tools":[],"prompt":"x"}"#),
            ("spawn", r#"{"name":"a","tools":["rm"],"prompt":"x"}"#),
            ("wake", r#"{"after_ms":100000000}"#),
        ] {
            assert!(
                ToolArgs::parse(&name(tool), &input(bad), &at).is_err(),
                "{tool} {bad}"
            );
        }
        let read_only = Boundary {
            tools: &[Tool::Read],
            ..all()
        };
        assert!(
            ToolArgs::parse(&name("tasks"), &input("{}"), &read_only).is_err(),
            "a tool the actor is not offered is invalid"
        );
    }

    #[test]
    fn tool_args_parse_once() {
        let name = |text: &str| ToolName::parse(text).expect("a tool name");
        let input = |text: &str| ToolInput::parse(text).expect("a JSON object");
        assert_eq!(
            ToolArgs::parse(&name("read"), &input(r#"{"path":"a.rs"}"#), &all()),
            Ok(ToolArgs::Read {
                path: ProjectPath::parse("a.rs").expect("a path")
            })
        );
        assert_eq!(
            ToolArgs::parse(&name("read"), &input(r#"{"path":"../a"}"#), &all()).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("read"), &input(r#"{"path":"a","x":1}"#), &all()).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(
                &name("edit"),
                &input(r#"{"path":"a","old":"","new":"b"}"#),
                &all()
            )
            .ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("bash"), &input(r#"{"command":"  "}"#), &all()).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("write"), &input("{}"), &all()).ok(),
            None,
            "rejected"
        );
    }

    #[test]
    fn tool_input_is_an_object_or_nothing() {
        assert_eq!(ToolInput::parse(""), Ok(ToolInput::default()), "no input");
        assert!(ToolInput::parse("[1]").is_err(), "not an object");
        assert!(ToolInput::parse("{\"path\":").is_err(), "not JSON");
    }

    #[test]
    fn records_round_trip_in_the_log_shape() {
        let record: Record<Parsed> = Record::ModelOutput {
            step: verified_domain::StepId {
                turn: verified_domain::TurnId(7),
                index: 0,
            },
            parts: measure(vec![ModelPart::ToolCall {
                tool_call_id: CallId::parse("toolu_1").expect("an id"),
                tool_name: ToolName::parse("read").expect("a name"),
                input: ToolInput::parse(r#"{"path":"a"}"#).expect("an object"),
            }]),
        };
        let json = serde_json::to_string(&record).expect("serializes");
        assert_eq!(
            json,
            r#"{"type":"ModelOutput","step":{"turn":"7","index":0},"parts":{"body":[{"type":"tool-call","toolCallId":"toolu_1","toolName":"read","input":{"path":"a"}}],"bytes":84}}"#,
            "the log's payload shape"
        );
        assert_eq!(
            serde_json::from_str::<Record<Parsed>>(&json).ok(),
            Some(record),
            "round trip"
        );
        let wrong_slot = r#"{"type":"ModelOutput","step":{"turn":"7","index":0},"parts":{"body":"hi","bytes":4}}"#;
        assert!(
            serde_json::from_str::<Record<Parsed>>(wrong_slot).is_err(),
            "text in the parts slot does not parse"
        );
    }
}
