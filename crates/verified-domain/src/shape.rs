//! What the core reads of a call's arguments: its tool, its subject, and
//! the few facts the runtime acts on.
//!
//! The rest of the arguments stay opaque payload. The subject is what a
//! rule's pattern matches: the path of a `read` or `edit`, the command of a
//! `bash`, the recipient's address of a `send`, the child's name of a
//! `spawn`. Everything here is parsed at the model boundary, so the core
//! never reads raw arguments.

use vstd::prelude::*;

use crate::id::{ActorId, TaskId};
use crate::record::Tool;

verus! {

/// A call's shape, parsed at the model boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(tag = "tool", rename_all = "camelCase")]
pub enum Shape {
    /// The path.
    Read { subject: Vec<u8> },
    /// The path.
    Edit { subject: Vec<u8> },
    /// The command; a background one runs on as a task.
    Bash { subject: Vec<u8>, background: bool },
    /// The child's name, and the id minted for it before dispatch.
    Spawn { subject: Vec<u8>, child: ActorId },
    /// The recipient's address, as text.
    Send { subject: Vec<u8> },
    /// A timer: always a task.
    Wake,
    /// The actor's task listing.
    Tasks,
    /// Cancel one of the actor's tasks.
    Cancel { task: TaskId },
}

/// A shape as the proofs see it.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `Seq` values that exist only while verifying")]
pub struct ShapeView {
    pub tool: Tool,
    /// Empty for a tool without one (`wake`, `tasks`, `cancel`).
    pub subject: Seq<u8>,
    /// Whether the call starts a task rather than being waited on.
    pub background: bool,
    /// The actor a `spawn` makes.
    pub child: Option<ActorId>,
    /// The task a `cancel` names.
    pub task: Option<TaskId>,
}

impl View for Shape {
    type V = ShapeView;

    open spec fn view(&self) -> ShapeView {
        match self {
            Shape::Read { subject } => ShapeView {
                tool: Tool::Read,
                subject: subject@,
                background: false,
                child: None,
                task: None,
            },
            Shape::Edit { subject } => ShapeView {
                tool: Tool::Edit,
                subject: subject@,
                background: false,
                child: None,
                task: None,
            },
            Shape::Bash { subject, background } => ShapeView {
                tool: Tool::Bash,
                subject: subject@,
                background: *background,
                child: None,
                task: None,
            },
            Shape::Spawn { subject, child } => ShapeView {
                tool: Tool::Spawn,
                subject: subject@,
                background: false,
                child: Some(*child),
                task: None,
            },
            Shape::Send { subject } => ShapeView {
                tool: Tool::Send,
                subject: subject@,
                background: false,
                child: None,
                task: None,
            },
            Shape::Wake => ShapeView {
                tool: Tool::Wake,
                subject: Seq::empty(),
                background: true,
                child: None,
                task: None,
            },
            Shape::Tasks => ShapeView {
                tool: Tool::Tasks,
                subject: Seq::empty(),
                background: false,
                child: None,
                task: None,
            },
            Shape::Cancel { task } => ShapeView {
                tool: Tool::Cancel,
                subject: Seq::empty(),
                background: false,
                child: None,
                task: Some(*task),
            },
        }
    }
}

impl Shape {
    /// The call's tool.
    #[must_use]
    pub const fn tool(&self) -> (tool: Tool)
        ensures
            tool == self@.tool,
    {
        match self {
            Shape::Read { .. } => Tool::Read,
            Shape::Edit { .. } => Tool::Edit,
            Shape::Bash { .. } => Tool::Bash,
            Shape::Spawn { .. } => Tool::Spawn,
            Shape::Send { .. } => Tool::Send,
            Shape::Wake => Tool::Wake,
            Shape::Tasks => Tool::Tasks,
            Shape::Cancel { .. } => Tool::Cancel,
        }
    }

    /// What a rule's pattern matches; `None` for a tool without one (its
    /// subject is empty).
    #[must_use]
    pub const fn subject(&self) -> (subject: Option<&Vec<u8>>)
        ensures
            match subject {
                Some(bytes) => bytes@ == self@.subject,
                None => self@.subject.len() == 0,
            },
    {
        match self {
            Shape::Read { subject }
            | Shape::Edit { subject }
            | Shape::Bash { subject, .. }
            | Shape::Spawn { subject, .. }
            | Shape::Send { subject } => Some(subject),
            Shape::Wake | Shape::Tasks | Shape::Cancel { .. } => None,
        }
    }

    /// Whether the call starts a task.
    #[must_use]
    pub const fn background(&self) -> (background: bool)
        ensures
            background == self@.background,
    {
        match self {
            Shape::Bash { background, .. } => *background,
            Shape::Wake => true,
            Shape::Read { .. }
            | Shape::Edit { .. }
            | Shape::Spawn { .. }
            | Shape::Send { .. }
            | Shape::Tasks
            | Shape::Cancel { .. } => false,
        }
    }

    /// The actor a `spawn` makes.
    #[must_use]
    pub const fn child(&self) -> (child: Option<ActorId>)
        ensures
            child == self@.child,
    {
        match self {
            Shape::Spawn { child, .. } => Some(*child),
            Shape::Read { .. }
            | Shape::Edit { .. }
            | Shape::Bash { .. }
            | Shape::Send { .. }
            | Shape::Wake
            | Shape::Tasks
            | Shape::Cancel { .. } => None,
        }
    }

    /// The task a `cancel` names.
    #[must_use]
    pub const fn task(&self) -> (task: Option<TaskId>)
        ensures
            task == self@.task,
    {
        match self {
            Shape::Cancel { task } => Some(*task),
            Shape::Read { .. }
            | Shape::Edit { .. }
            | Shape::Bash { .. }
            | Shape::Spawn { .. }
            | Shape::Send { .. }
            | Shape::Wake
            | Shape::Tasks => None,
        }
    }

    /// A copy with the same view.
    #[must_use]
    pub fn copy(&self) -> (copy: Shape)
        ensures
            copy@ == self@,
    {
        match self {
            Shape::Read { subject } => Shape::Read { subject: subject.clone() },
            Shape::Edit { subject } => Shape::Edit { subject: subject.clone() },
            Shape::Bash { subject, background } => Shape::Bash { subject: subject.clone(), background: *background },
            Shape::Spawn { subject, child } => Shape::Spawn { subject: subject.clone(), child: *child },
            Shape::Send { subject } => Shape::Send { subject: subject.clone() },
            Shape::Wake => Shape::Wake,
            Shape::Tasks => Shape::Tasks,
            Shape::Cancel { task } => Shape::Cancel { task: *task },
        }
    }
}

} // verus!

#[expect(
    clippy::multiple_inherent_impl,
    reason = "plain Rust beside the impl inside `verus!`: no proof mentions it"
)]
impl Shape {
    /// The shape of a foreground call of `tool` with no subject, and for a
    /// `spawn` or `cancel` the given child and task: what a test or the
    /// model checker proposes.
    #[must_use]
    pub const fn plain(tool: Tool, child: ActorId, task: TaskId) -> Self {
        match tool {
            Tool::Read => Self::Read {
                subject: Vec::new(),
            },
            Tool::Edit => Self::Edit {
                subject: Vec::new(),
            },
            Tool::Bash => Self::Bash {
                subject: Vec::new(),
                background: false,
            },
            Tool::Spawn => Self::Spawn {
                subject: Vec::new(),
                child,
            },
            Tool::Send => Self::Send {
                subject: Vec::new(),
            },
            Tool::Wake => Self::Wake,
            Tool::Tasks => Self::Tasks,
            Tool::Cancel => Self::Cancel { task },
        }
    }

    /// This shape with its subject dropped: what the model checker keeps.
    #[must_use]
    pub const fn without_subject(&self) -> Self {
        match self {
            Self::Read { .. } => Self::Read {
                subject: Vec::new(),
            },
            Self::Edit { .. } => Self::Edit {
                subject: Vec::new(),
            },
            Self::Bash { background, .. } => Self::Bash {
                subject: Vec::new(),
                background: *background,
            },
            Self::Spawn { child, .. } => Self::Spawn {
                subject: Vec::new(),
                child: *child,
            },
            Self::Send { .. } => Self::Send {
                subject: Vec::new(),
            },
            Self::Wake => Self::Wake,
            Self::Tasks => Self::Tasks,
            Self::Cancel { task } => Self::Cancel { task: *task },
        }
    }
}
