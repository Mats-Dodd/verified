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
use crate::shape::Shape;
#[cfg(verus_only)]
use crate::shape::ShapeView;

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

    /// The tool and subject of `args`, as the proofs see them.
    spec fn spec_shape(args: &Self::Args) -> ShapeView;

    /// The tool and subject of `args`: arguments name their tool, so a call
    /// cannot say one tool and carry another's arguments.
    fn shape(args: &Self::Args) -> (shape: Shape)
        ensures
            shape@ == Self::spec_shape(args),
    ;

    /// The tool `args` are for.
    fn tool(args: &Self::Args) -> (tool: Tool)
        ensures
            tool == Self::spec_shape(args).tool,
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
/// arguments only its tool (its subject is empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[verifier::external_derive]
pub struct Abstract;

impl Payload for Abstract {
    type Text = Unit;
    type Parts = Unit;
    type Args = Tool;
    type Reason = Unit;
    type Output = Unit;

    open spec fn spec_shape(args: &Tool) -> ShapeView {
        ShapeView { tool: *args, subject: Seq::empty() }
    }

    fn shape(args: &Tool) -> (shape: Shape) {
        let shape = Shape { tool: *args, subject: Vec::new() };
        assert(shape@ =~= Self::spec_shape(args));
        shape
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
