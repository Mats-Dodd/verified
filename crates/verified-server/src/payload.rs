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
use verified_domain::id::{CallId, SessionId, ToolName};
use verified_domain::{Measured, Payload, Proposal, Request, Tool};
use verified_wire::chunk::FinishReason;

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
///
/// Its length leaves room for a [`Sibling`] suffix, so the staged sibling
/// of any parsed path is itself a valid path by construction
/// ([`ProjectPath::sibling`] cannot fail).
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

/// What follows a path's name in a sibling's: `[A-Za-z0-9_-]`, at most
/// [`Sibling::MAX_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sibling(String);

impl Sibling {
    /// The longest suffix.
    pub const MAX_BYTES: usize = 128;

    /// # Errors
    /// Empty, longer than [`Sibling::MAX_BYTES`], or a character outside
    /// `[A-Za-z0-9_-]`.
    pub fn parse(text: &str) -> Result<Self, PathError> {
        if text.is_empty() {
            return Err(PathError::Empty);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(PathError::TooLong {
                limit: Self::MAX_BYTES,
            });
        }
        if !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(PathError::NotNormal);
        }
        Ok(Self(text.to_owned()))
    }

    /// The suffix of the staged file of the edit dispatched by record `seq`
    /// of `session`: `verified-<session>-<seq>`. Infallible: a session id's
    /// text is lowercase hex and `-`, a seq is decimal digits, and together
    /// with the prefix they take at most 9 + 36 + 1 + 20 = 66 bytes.
    #[must_use]
    pub fn staged(session: SessionId, seq: u64) -> Self {
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
    /// The longest path accepted, in bytes: room is left below
    /// [`ProjectPath::SIBLING_MAX_BYTES`] for a sibling's `.` + name + `.` +
    /// [`Sibling`].
    pub const MAX_BYTES: usize = 3840;

    /// The longest a sibling of a parsed path can be.
    pub const SIBLING_MAX_BYTES: usize = 4096;

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

    /// The sibling of this path named `.{name}.{suffix}`: same directory, so
    /// a rename onto this path is atomic.
    ///
    /// Infallible by construction: this path is at most
    /// [`ProjectPath::MAX_BYTES`] and `suffix` at most [`Sibling::MAX_BYTES`],
    /// so the sibling fits [`ProjectPath::SIBLING_MAX_BYTES`]; its directory
    /// is this path's, and its last component (`.`, a non-empty name, `.`, a
    /// non-empty suffix of `[A-Za-z0-9_-]`) is neither empty, `.`, `..`, the
    /// state directory, nor anything with a `/` or NUL. Staged names are
    /// reserved, so it never names a path the tools accept.
    #[must_use]
    pub fn sibling(&self, suffix: &Sibling) -> Self {
        let (dir, name) = self
            .0
            .rsplit_once('/')
            .map_or(("", self.0.as_str()), |(dir, name)| (dir, name));
        let suffix = suffix.as_str();
        Self(if dir.is_empty() {
            format!(".{name}.{suffix}")
        } else {
            format!("{dir}/.{name}.{suffix}")
        })
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
    /// Run a shell command in the project.
    Bash { command: Command },
}

impl ToolArgs {
    #[must_use]
    pub const fn tool(&self) -> Tool {
        match self {
            Self::Read { .. } => Tool::Read,
            Self::Edit { .. } => Tool::Edit,
            Self::Bash { .. } => Tool::Bash,
        }
    }

    /// Parses a tool call from the model. The error is the reason the model
    /// is told.
    ///
    /// # Errors
    /// An unknown tool, input that is not the tool's JSON, or an invalid field.
    pub fn parse(name: &ToolName, input: &ToolInput) -> Result<Self, String> {
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
        }

        let bad = |error: serde_json::Error| format!("invalid input for `{name}`: {error}");
        let input = input.to_value();
        let path = |text: &str| {
            ProjectPath::parse(text).map_err(|error| format!("invalid path `{text}`: {error}"))
        };
        match name.as_str() {
            "read" => {
                let input: ReadInput = serde_json::from_value(input).map_err(bad)?;
                Ok(Self::Read {
                    path: path(&input.path)?,
                })
            }
            "edit" => {
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
            "bash" => {
                let input: BashInput = serde_json::from_value(input).map_err(bad)?;
                Ok(Self::Bash {
                    command: Command::parse(input.command)?,
                })
            }
            other => Err(format!("unknown tool `{other}`")),
        }
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
pub fn proposal(id: &CallId, name: &ToolName, input: &ToolInput) -> Proposal<Parsed> {
    let request = match ToolArgs::parse(name, input) {
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
    use proptest::prelude::*;

    use super::{ModelPart, Parsed, PathError, ProjectPath, Sibling, ToolArgs, ToolInput, measure};
    use verified_domain::Record;
    use verified_domain::id::{CallId, SessionId, ToolName};

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
        let path = ProjectPath::parse("src/lib.rs").expect("a path");
        let suffix = Sibling::parse("tmp").expect("a suffix");
        assert_eq!(
            path.sibling(&suffix).as_str(),
            "src/.lib.rs.tmp",
            "same directory"
        );
        assert_eq!(
            ProjectPath::parse(&"a".repeat(ProjectPath::MAX_BYTES + 1)),
            Err(PathError::TooLong {
                limit: ProjectPath::MAX_BYTES
            }),
            "too long"
        );
    }

    /// Any path that parses has a sibling for any suffix that parses: the
    /// staged name of an approved edit never fails to exist (it used to
    /// panic for a long path, and a panic in a rerun after recovery is a
    /// crash loop).
    #[test]
    fn the_longest_path_has_a_sibling() {
        let longest = format!(
            "{}/{}",
            "d".repeat(100),
            "n".repeat(ProjectPath::MAX_BYTES - 101)
        );
        let path = ProjectPath::parse(&longest).expect("the longest path parses");
        let suffix = Sibling::parse(&"s".repeat(Sibling::MAX_BYTES)).expect("the longest suffix");
        let sibling = path.sibling(&suffix);
        assert!(
            sibling.as_str().len() <= ProjectPath::SIBLING_MAX_BYTES,
            "it fits"
        );
        assert!(
            sibling.as_str().starts_with(&"d".repeat(100)),
            "same directory"
        );
        let session = SessionId::parse("01900000-0000-7000-8000-000000000001").expect("a v7");
        let staged = Sibling::staged(session, u64::MAX);
        assert_eq!(
            Sibling::parse(staged.as_str()),
            Ok(staged.clone()),
            "a valid suffix"
        );
        let short = ProjectPath::parse("src/lib.rs").expect("a path");
        assert_eq!(
            ProjectPath::parse(short.sibling(&staged).as_str()),
            Err(PathError::Reserved),
            "no tool may name a staged file"
        );
    }

    proptest! {
        #[test]
        fn siblings_stay_beside_their_path(
            dirs in prop::collection::vec("[a-z]{1,8}", 0..4),
            name in "[a-zA-Z0-9_.-]{1,40}",
            suffix in "[a-zA-Z0-9_-]{1,128}",
        ) {
            let mut text = dirs.join("/");
            if !text.is_empty() {
                text.push('/');
            }
            text.push_str(&name);
            if let (Ok(path), Ok(suffix)) = (ProjectPath::parse(&text), Sibling::parse(&suffix)) {
                let sibling = path.sibling(&suffix);
                let expected = format!(".{name}.{}", suffix.as_str());
                prop_assert_eq!(sibling.as_str().rsplit('/').next(), Some(expected.as_str()));
                prop_assert!(sibling.as_str().len() <= ProjectPath::SIBLING_MAX_BYTES);
                prop_assert!(!sibling.as_str().contains('\0'));
                prop_assert!(sibling.as_str().split('/').all(|part| !part.is_empty() && part != "." && part != ".."));
            }
        }
    }

    #[test]
    fn tool_args_parse_once() {
        let name = |text: &str| ToolName::parse(text).expect("a tool name");
        let input = |text: &str| ToolInput::parse(text).expect("a JSON object");
        assert_eq!(
            ToolArgs::parse(&name("read"), &input(r#"{"path":"a.rs"}"#)),
            Ok(ToolArgs::Read {
                path: ProjectPath::parse("a.rs").expect("a path")
            })
        );
        assert_eq!(
            ToolArgs::parse(&name("read"), &input(r#"{"path":"../a"}"#)).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("read"), &input(r#"{"path":"a","x":1}"#)).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("edit"), &input(r#"{"path":"a","old":"","new":"b"}"#)).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("bash"), &input(r#"{"command":"  "}"#)).ok(),
            None,
            "rejected"
        );
        assert_eq!(
            ToolArgs::parse(&name("write"), &input("{}")).ok(),
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
                turn: verified_domain::TurnId::parse("01900000-0000-7000-8000-000000000001")
                    .expect("a v7"),
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
            r#"{"type":"ModelOutput","step":{"turn":"01900000-0000-7000-8000-000000000001","index":0},"parts":{"body":[{"type":"tool-call","toolCallId":"toolu_1","toolName":"read","input":{"path":"a"}}],"bytes":84}}"#,
            "the log's payload shape"
        );
        assert_eq!(
            serde_json::from_str::<Record<Parsed>>(&json).ok(),
            Some(record),
            "round trip"
        );
        let wrong_slot = r#"{"type":"ModelOutput","step":{"turn":"01900000-0000-7000-8000-000000000001","index":0},"parts":{"body":"hi","bytes":4}}"#;
        assert!(
            serde_json::from_str::<Record<Parsed>>(wrong_slot).is_err(),
            "text in the parts slot does not parse"
        );
    }
}
