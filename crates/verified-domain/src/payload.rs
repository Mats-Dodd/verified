//! What a record or event carries besides ids: one associated type per slot.
//!
//! The core never reads a payload; it moves each one from the event that
//! brought it into the record that keeps it, and asks only which tool a
//! call's arguments are for. Production fills the slots with parsed types
//! (the user's text, raw model parts, `ToolArgs`, a reason, a tool's output);
//! the model checker fills them with [`Unit`] and lets a call's arguments be
//! just its [`Tool`]. Because every slot has its own type, a record with the
//! wrong kind of payload in a slot does not type-check or deserialize.

use core::fmt;
use core::hash::Hash;

use serde::Serialize;
use serde::de::DeserializeOwned;
use vstd::prelude::*;

use crate::record::Tool;

verus! {

/// The types that fill each slot. Every slot is plain data: cloneable,
/// comparable and hashable, so records are too.
pub trait Payload {
    /// What the user typed.
    type Text: Clone + fmt::Debug + PartialEq + Eq + Hash;
    /// A batch of model stream parts, exactly as streamed.
    type Parts: Clone + fmt::Debug + PartialEq + Eq + Hash;
    /// A tool call's arguments, parsed at the model boundary.
    type Args: Clone + fmt::Debug + PartialEq + Eq + Hash;
    /// Why a proposed call is invalid; the model is told.
    type Reason: Clone + fmt::Debug + PartialEq + Eq + Hash;
    /// What a tool returned.
    type Output: Clone + fmt::Debug + PartialEq + Eq + Hash;

    /// The tool `args` are for, as the proofs see it.
    spec fn spec_tool(args: &Self::Args) -> Tool;

    /// The tool `args` are for: arguments name their tool, so a call cannot
    /// say one tool and carry another's arguments.
    fn tool(args: &Self::Args) -> (tool: Tool)
        ensures
            tool == Self::spec_tool(args),
    ;
}

/// A payload and its size in bytes, counted against
/// `Limits::context_bytes`. The boundary that parses the payload measures it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Measured<T> {
    pub body: T,
    pub bytes: u64,
}

/// An empty slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Unit;

/// The payload the model checker runs: every slot empty, and a call's
/// arguments only its tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[verifier::external_derive]
pub struct Abstract;

impl Payload for Abstract {
    type Text = Unit;
    type Parts = Unit;
    type Args = Tool;
    type Reason = Unit;
    type Output = Unit;

    open spec fn spec_tool(args: &Tool) -> Tool {
        *args
    }

    fn tool(args: &Tool) -> (tool: Tool) {
        *args
    }
}

} // verus!

/// A [`Payload`] whose every slot round-trips through serde: what a log
/// line or a test fixture needs.
pub trait Codec:
    Payload<
        Text: Serialize + DeserializeOwned,
        Parts: Serialize + DeserializeOwned,
        Args: Serialize + DeserializeOwned,
        Reason: Serialize + DeserializeOwned,
        Output: Serialize + DeserializeOwned,
    >
{
}

impl<P> Codec for P where
    P: Payload<
            Text: Serialize + DeserializeOwned,
            Parts: Serialize + DeserializeOwned,
            Args: Serialize + DeserializeOwned,
            Reason: Serialize + DeserializeOwned,
            Output: Serialize + DeserializeOwned,
        >
{
}

/// α, the abstraction from any payload to [`Abstract`].
///
/// A record keeps every id and every size, drops each body, and reduces a
/// call's arguments to the tool they name. The core reads nothing else of
/// a payload (it compares sizes against `Limits::context_bytes` and asks a
/// call's tool), so a core run on α of a log decides α of what it decides
/// on the log; `verified-model`'s `alpha` test checks that.
pub mod alpha {
    use super::{Abstract, Measured, Payload, Unit};
    use crate::record::{Proposal, Record, Request};

    /// A payload's size without its body.
    #[must_use]
    pub const fn measured<T>(measured: &Measured<T>) -> Measured<Unit> {
        Measured {
            body: Unit,
            bytes: measured.bytes,
        }
    }

    /// A proposal's id and the tool its arguments name; an invalid request
    /// stays invalid.
    #[must_use]
    pub fn proposal<P: Payload>(proposal: &Proposal<P>) -> Proposal<Abstract> {
        Proposal {
            id: proposal.id.clone(),
            request: match &proposal.request {
                Request::Run { args } => Request::Run {
                    args: P::tool(args),
                },
                Request::Invalid { .. } => Request::Invalid { reason: Unit },
            },
        }
    }

    #[must_use]
    pub fn record<P: Payload>(record: &Record<P>) -> Record<Abstract> {
        match record {
            Record::TurnStarted {
                turn,
                message,
                text,
            } => Record::TurnStarted {
                turn: *turn,
                message: *message,
                text: measured(text),
            },
            Record::StepStarted { step } => Record::StepStarted { step: *step },
            Record::ModelOutput { step, parts } => Record::ModelOutput {
                step: *step,
                parts: measured(parts),
            },
            Record::StepReset { step, reason } => Record::StepReset {
                step: *step,
                reason: *reason,
            },
            Record::StepFinished { step, calls } => Record::StepFinished {
                step: *step,
                calls: calls.iter().map(proposal).collect(),
            },
            Record::Approved { call, by } => Record::Approved {
                call: *call,
                by: *by,
            },
            Record::Dispatched { call } => Record::Dispatched { call: *call },
            Record::ToolResult { call, exit, output } => Record::ToolResult {
                call: *call,
                exit: *exit,
                output: output.as_ref().map(measured),
            },
            Record::CancelRequested { turn } => Record::CancelRequested { turn: *turn },
            Record::TurnEnded { turn, outcome } => Record::TurnEnded {
                turn: *turn,
                outcome: *outcome,
            },
            Record::EpochBumped { epoch } => Record::EpochBumped { epoch: *epoch },
        }
    }
}
