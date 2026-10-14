//! What the core reads of a call's arguments: its tool and its subject.
//!
//! The rest of the arguments stay opaque payload. The subject is what a
//! rule's pattern matches: the path of a `read` or `edit`, the command of a
//! `bash`.

use vstd::prelude::*;

use crate::record::Tool;

verus! {

/// A call's tool and subject, parsed at the model boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Shape {
    pub tool: Tool,
    pub subject: Vec<u8>,
}

/// A shape as the proofs see it.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `Seq` values that exist only while verifying")]
pub struct ShapeView {
    pub tool: Tool,
    pub subject: Seq<u8>,
}

impl View for Shape {
    type V = ShapeView;

    open spec fn view(&self) -> ShapeView {
        ShapeView { tool: self.tool, subject: self.subject@ }
    }
}

} // verus!
