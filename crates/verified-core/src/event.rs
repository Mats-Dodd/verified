//! Events the shell feeds the core: parsed user commands and the completions
//! of effects. Everything nondeterministic (ids, sizes, what the model said)
//! arrives here as data.

use vstd::prelude::*;

use verified_domain::{
    Born, CallId, EffectId, Envelope, Exit, Measured, ModelFault, Payload, Proposal, Rules, TurnId,
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
    /// core refuses a response that reuses a call id.
    Finished(Vec<Proposal<P>>),
    Failed(ModelFault),
}

/// How a summarize call ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Summary<P: Payload> {
    /// The model wrote this summary.
    Written(Measured<P::Text>),
    Failed(ModelFault),
}

/// The rules the shell read for `Core::resume`, with the text of the file
/// they came from (`None`: there is no rules file, so the default rules).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Loaded<P: Payload> {
    pub rules: Rules,
    pub file: Option<Measured<P::Text>>,
}

/// One input to `Core::decide`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Event<P: Payload> {
    /// Who this actor is: on an unborn core, recorded once.
    Born { born: Born },
    /// The rules file was read and differs from the rules in force.
    Rules { rules: Rules, file: Option<Measured<P::Text>> },
    /// An envelope for this actor's mailbox: a user's message or stop, an
    /// agent's message, a parent's stop. A repeat is recognised by its id.
    Deliver { envelope: Envelope<P> },
    /// The user answered an approval request.
    Approve { call: CallId, verdict: Verdict },
    /// The user asked to stop a turn.
    Interrupt { turn: TurnId },
    /// A batch of streamed model parts (group commit is the shell's).
    ModelParts { id: EffectId, parts: Measured<P::Parts> },
    /// A `CallModel` effect ended. The shell delivers every batch of parts
    /// before this.
    ModelDone { id: EffectId, result: ModelExit<P> },
    /// A `RunTool` or `Stop` effect ended: a call's run, a task's, or a
    /// delivery's.
    ToolDone { id: EffectId, exit: Exit, output: Measured<P::Output> },
    /// A `Sleep` effect ended.
    Woke { id: EffectId },
    /// The user asked to compact the model's history (`/compact`).
    Compact,
    /// A `Summarize` effect ended.
    Summarized { id: EffectId, summary: Summary<P> },
    /// The deadline of run `id` (a call's or a task's) passed: v stops
    /// waiting. A timeout is v cancelling at a deadline, so a run still
    /// unresolved is `Unknown` and asked to stop; one that already reported
    /// keeps its outcome.
    Expired { id: EffectId },
}

} // verus!

impl<P: Payload> Loaded<P> {
    /// No rules file: the default rules, `read: allow`.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            rules: Rules::default(),
            file: None,
        }
    }
}
