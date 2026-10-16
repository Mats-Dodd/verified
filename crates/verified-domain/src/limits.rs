//! The `Limits` value made once at startup that bounds every collection,
//! loop and retry. It is data: the core reads it, never grows past it, and
//! turns an exceeded bound into a typed outcome.
//!
//! A `Limits` is only made by [`Limits::new`] from a [`LimitsConfig`]: every
//! count is non-zero by type, so nothing downstream patches a value at run
//! time.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use vstd::prelude::*;

verus! {

/// A duration in milliseconds. Time enters the core only as data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Millis(u64);

impl Millis {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Every bound the agent runs under. Its fields are private: a value comes
/// only from [`Limits::new`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Limits {
    turns_per_actor: usize,
    messages_per_actor: usize,
    tasks_per_actor: usize,
    children_per_actor: usize,
    depth: u32,
    steps_per_turn: u32,
    calls_per_step: usize,
    calls_per_turn: usize,
    context_bytes: u64,
    retry_schedule: Vec<Millis>,
    parts_per_batch: usize,
    batch_window: Millis,
}

impl Limits {
    /// Turns one actor ever starts.
    #[must_use]
    pub const fn turns_per_actor(&self) -> usize {
        self.turns_per_actor
    }

    /// Envelopes one actor ever accepts into its mailbox.
    #[must_use]
    pub const fn messages_per_actor(&self) -> usize {
        self.messages_per_actor
    }

    /// Tasks one actor ever starts; a background call past it does not
    /// start.
    #[must_use]
    pub const fn tasks_per_actor(&self) -> usize {
        self.tasks_per_actor
    }

    /// Children one actor ever spawns; a `spawn` past it resolves
    /// `NotStarted(Limit)`, as a background call past `tasks_per_actor`
    /// does. It bounds the core's list of children, and so what a
    /// `Compacted` lists as open.
    #[must_use]
    pub const fn children_per_actor(&self) -> usize {
        self.children_per_actor
    }

    /// The deepest spawn: a root actor is at depth 0.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// Model calls one turn may start (retries of a call do not count).
    #[must_use]
    pub const fn steps_per_turn(&self) -> u32 {
        self.steps_per_turn
    }

    /// Tool calls one model response may propose; those past the bound fail
    /// without running.
    #[must_use]
    pub const fn calls_per_step(&self) -> usize {
        self.calls_per_step
    }

    /// Tool calls one turn may propose across its steps; those past the bound
    /// fail without running.
    #[must_use]
    pub const fn calls_per_turn(&self) -> usize {
        self.calls_per_turn
    }

    /// Conversation size a step is sent; a step that would exceed it
    /// compacts the history first, and fails before the call if no summary
    /// makes room. A step's own output and its tools' may carry the
    /// conversation past it; the next beat then compacts.
    #[must_use]
    pub const fn context_bytes(&self) -> u64 {
        self.context_bytes
    }

    /// The backoff before retry `attempt` (from 0) of an overloaded or
    /// network-failed model call; `None` once the schedule is spent.
    #[must_use]
    #[expect(clippy::indexing_slicing, reason = "Verus proves the index is below the schedule's length")]
    pub fn retry_delay(&self, attempt: usize) -> (delay: Option<Millis>)
        ensures
            delay is Some ==> attempt < usize::MAX,
    {
        if attempt < self.retry_schedule.len() {
            Some(self.retry_schedule[attempt])
        } else {
            None
        }
    }

    /// Group commit: the longest a model part waits for its batch.
    #[must_use]
    pub const fn batch_window(&self) -> Millis {
        self.batch_window
    }
}

} // verus!

#[expect(
    clippy::multiple_inherent_impl,
    reason = "plain Rust beside the impl inside `verus!`: no proof mentions it"
)]
impl Limits {
    /// The bounds the agent runs under.
    #[must_use]
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            turns_per_actor: config.turns_per_actor.get(),
            messages_per_actor: config.messages_per_actor.get(),
            tasks_per_actor: config.tasks_per_actor.get(),
            children_per_actor: config.children_per_actor.get(),
            depth: config.depth,
            steps_per_turn: config.steps_per_turn.get(),
            calls_per_step: config.calls_per_step.get(),
            calls_per_turn: config.calls_per_turn.get(),
            context_bytes: config.context_bytes.get(),
            retry_schedule: config.retry_schedule,
            parts_per_batch: config.parts_per_batch.get(),
            batch_window: config.batch_window,
        }
    }

    /// The bounds v0 ships with.
    #[must_use]
    pub fn v0() -> Self {
        Self::new(LimitsConfig::v0())
    }

    /// Group commit: model parts batched into one record before an fsync.
    ///
    /// # Panics
    /// Never: [`Limits::new`] made it from a non-zero count.
    #[must_use]
    pub const fn parts_per_batch(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.parts_per_batch).expect("made from a NonZeroUsize")
    }
}

/// The bounds a [`Limits`] is made from. Every count is non-zero by type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LimitsConfig {
    pub turns_per_actor: NonZeroUsize,
    pub messages_per_actor: NonZeroUsize,
    pub tasks_per_actor: NonZeroUsize,
    pub children_per_actor: NonZeroUsize,
    pub depth: u32,
    pub steps_per_turn: NonZeroU32,
    pub calls_per_step: NonZeroUsize,
    pub calls_per_turn: NonZeroUsize,
    pub context_bytes: NonZeroU64,
    /// Its length is the retry budget of one step.
    pub retry_schedule: Vec<Millis>,
    /// Group commit: model parts batched into one record before an fsync.
    pub parts_per_batch: NonZeroUsize,
    pub batch_window: Millis,
}

impl LimitsConfig {
    /// The bounds v0 ships with.
    ///
    /// # Panics
    /// Never: every count is a non-zero constant.
    #[must_use]
    pub fn v0() -> Self {
        let nonzero = "a non-zero constant";
        Self {
            turns_per_actor: NonZeroUsize::new(1024).expect(nonzero),
            messages_per_actor: NonZeroUsize::new(1024).expect(nonzero),
            tasks_per_actor: NonZeroUsize::new(64).expect(nonzero),
            children_per_actor: NonZeroUsize::new(64).expect(nonzero),
            depth: 3,
            steps_per_turn: NonZeroU32::new(64).expect(nonzero),
            calls_per_step: NonZeroUsize::new(16).expect(nonzero),
            calls_per_turn: NonZeroUsize::new(256).expect(nonzero),
            context_bytes: NonZeroU64::new(600_000).expect(nonzero),
            retry_schedule: vec![Millis::new(500), Millis::new(2_000), Millis::new(8_000)],
            parts_per_batch: NonZeroUsize::new(64).expect(nonzero),
            batch_window: Millis::new(30),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Limits, Millis};

    #[test]
    fn the_retry_schedule_runs_out() {
        assert_eq!(Limits::v0().retry_delay(3), None, "schedule spent");
        assert_eq!(Limits::v0().retry_delay(0), Some(Millis::new(500)));
    }
}
