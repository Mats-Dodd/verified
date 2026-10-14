//! Effects the core asks the shell to perform, each named by an `EffectId`,
//! and the `Decision` that carries them.
//!
//! A `ToolCall<Approved, P>` can only be made inside this crate, and the core
//! makes one only for a call whose `Approved` and `Dispatched` records are
//! durable once its decision is (`Core::decide`'s specification). The
//! approval is bound to its call: the token names the call's index and the
//! effect that runs it, and the shell derives everything a mutation may
//! touch from the call itself, so an approval for one call cannot authorize
//! another.

use vstd::prelude::*;

use verified_domain::{ActorId, CallId, CallIx, EffectId, Millis, Payload, Record, StepId, Tool};

verus! {

/// Typestate: the call awaits the user.
#[derive(Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
#[expect(missing_copy_implementations, reason = "typestate tokens are minted by the core, never copied")]
pub struct Pending {
    seal: (),
}

/// Typestate: the call was approved and dispatched, and both records are
/// durable before the effect carrying it starts. Not `Clone`, not
/// deserializable: only the core mints one, for the effect `effect`.
#[derive(Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
#[expect(missing_copy_implementations, reason = "an approval is minted once per dispatch and must not be duplicated")]
pub struct Approved {
    effect: EffectId,
}

/// A tool call in approval state `S`. Its fields are private: what an
/// approved call may do is fixed when the core mints it.
#[derive(Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct ToolCall<S, P: Payload> {
    call: CallIx,
    id: CallId,
    args: P::Args,
    state: S,
}

impl<S, P: Payload> ToolCall<S, P> {
    /// The call's index in its turn.
    pub closed spec fn spec_call(&self) -> CallIx {
        self.call
    }

    #[must_use]
    pub const fn call(&self) -> (call: CallIx)
        ensures
            call == self.spec_call(),
    {
        self.call
    }

    /// The model's `tool_use` id.
    #[must_use]
    pub const fn id(&self) -> &CallId {
        &self.id
    }

    #[must_use]
    pub fn tool(&self) -> Tool {
        P::tool(&self.args)
    }

    #[must_use]
    pub const fn args(&self) -> &P::Args {
        &self.args
    }
}

impl<P: Payload> ToolCall<Approved, P> {
    /// The effect that runs this call.
    pub closed spec fn spec_effect(&self) -> EffectId {
        self.state.effect
    }

    /// The effect that runs this call: its `RunTool` id.
    #[must_use]
    pub const fn effect(&self) -> (effect: EffectId)
        ensures
            effect == self.spec_effect(),
    {
        self.state.effect
    }

    /// The key a `Keyed` call's target deduplicates on: the seq of the
    /// `Dispatched` record that licensed it. A rerun after a crash keeps it
    /// (only the epoch changes), so the target accepts the call once (O2).
    #[must_use]
    pub const fn key(&self) -> (key: u64)
        ensures
            key == self.spec_effect().seq,
    {
        self.state.effect.seq
    }
}

/// Makes a call awaiting approval.
pub(crate) const fn pending<P: Payload>(call: CallIx, id: CallId, args: P::Args) -> ToolCall<Pending, P> {
    ToolCall { call, id, args, state: Pending { seal: () } }
}

/// The only place an approved call is minted. `decide` and `resume` call it
/// only for a call whose phase, once the decision's records apply, is
/// `Dispatched` by the record at `effect.seq`, in `effect.epoch`.
pub(crate) const fn approved<P: Payload>(
    call: CallIx,
    id: CallId,
    args: P::Args,
    effect: EffectId,
) -> (approved: ToolCall<Approved, P>)
    ensures
        approved.spec_call() == call,
        approved.spec_effect() == effect,
{
    ToolCall { call, id, args, state: Approved { effect } }
}

/// Work for the shell. Effects start only after every record of their
/// decision is durable and applied.
#[derive(Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Effect<P: Payload> {
    /// Stream the next model response for `step`. The shell builds the
    /// request from the log; it answers with `ModelParts` then `ModelDone`.
    CallModel { id: EffectId, step: StepId },
    /// Stop reading the stream of model call `id` and report `ModelDone`.
    CancelModel { id: EffectId },
    /// Tell the user `call` awaits approval; answered by an `Approve` event.
    AskApproval { request: ToolCall<Pending, P> },
    /// Run an approved call (a foreground run, a task's run, a `Keyed`
    /// delivery, a timer); answered by `ToolDone` for `call.effect()`.
    RunTool { call: ToolCall<Approved, P> },
    /// Ask tool run `id` to stop at its next safe point; it still answers.
    CancelTool { id: EffectId },
    /// Wait `delay` on the Clock seam; answered by `Woke`.
    Sleep { id: EffectId, delay: Millis },
    /// Deliver a stop to `child`, keyed by `id.seq` (its `StopSent`);
    /// answered by `ToolDone` for `id`.
    Stop { id: EffectId, child: ActorId },
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Rejection {
    /// The actor is stopped: it takes no more envelopes.
    Stopped,
    /// A bound: `Limits::messages_per_actor`, or a spawn past
    /// `Limits::depth`.
    Limit,
    /// The actor is not born yet.
    Unborn,
    /// The envelope is for another actor, or the actor was born otherwise.
    Mismatch,
    /// No call with that id awaits approval in the running step.
    UnknownCall,
    /// No turn with that id is running.
    NotRunning,
}

/// What `decide` made of its event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub enum Outcome {
    Accepted,
    /// A repeat of something already done (same envelope, interrupt of a
    /// turn already stopping, the same birth).
    Exists,
    Rejected(Rejection),
    /// A completion that matches nothing in flight: an earlier epoch, a
    /// late duplicate, or a superseded attempt.
    Stale,
}

/// The result of `decide` or `resume`: persist `records` in order with one
/// fsync, `apply` each, then start `effects`.
#[must_use]
#[derive(Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Decision<P: Payload> {
    pub records: Vec<Record<P>>,
    pub effects: Vec<Effect<P>>,
    pub outcome: Outcome,
}

} // verus!
