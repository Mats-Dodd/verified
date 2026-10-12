//! Events the shell feeds the core: parsed user commands and the completions
//! of effects. Everything nondeterministic (ids, sizes, what the model said)
//! arrives here as data.

use vstd::prelude::*;

use verified_domain::{
    CallId, Cause, EffectId, Exit, Measured, MessageId, Payload, Proposal, TurnId,
};

verus! {

/// The user's answer to an approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Verdict {
    Approve,
    Deny,
}

/// How a model call ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum ModelExit<P: Payload> {
    /// The response finished, proposing these calls (possibly none). The
    /// model boundary already refused a response that reuses a call id.
    Finished(Vec<Proposal<P>>),
    Failed(Cause),
}

/// One input to `Core::decide`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Event<P: Payload> {
    /// The user sent a message. Both ids were made by the client.
    Message { message: MessageId, turn: TurnId, text: Measured<P::Text> },
    /// The user answered an approval request.
    Approve { call: CallId, verdict: Verdict },
    /// The user asked to stop a turn.
    Interrupt { turn: TurnId },
    /// A batch of streamed model parts (group commit is the shell's).
    ModelParts { id: EffectId, parts: Measured<P::Parts> },
    /// A `CallModel` effect ended. The shell delivers every batch of parts
    /// before this.
    ModelDone { id: EffectId, result: ModelExit<P> },
    /// A `RunTool` effect ended.
    ToolDone { id: EffectId, exit: Exit, output: Measured<P::Output> },
    /// A `Sleep` effect ended.
    Woke { id: EffectId },
}

} // verus!
