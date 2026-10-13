//! The `Limits` value parsed once at startup that bounds every collection,
//! loop and retry. It is data: the core reads it, never grows past it, and
//! turns an exceeded bound into a typed outcome.
//!
//! A `Limits` is only made by [`Limits::parse`] from a [`LimitsConfig`]: every
//! count is non-zero by type and the cross-field rules are checked there, so
//! nothing downstream patches a value at run time.

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
/// only from [`Limits::parse`], which has checked them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Limits {
    turns_per_session: usize,
    steps_per_turn: u32,
    calls_per_step: usize,
    calls_per_turn: usize,
    context_bytes: u64,
    retry_schedule: Vec<Millis>,
    parts_per_batch: usize,
    batch_window: Millis,
}

impl Limits {
    /// Messages one session accepts.
    #[must_use]
    pub const fn turns_per_session(&self) -> usize {
        self.turns_per_session
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

    /// Conversation size the model is sent; a step that would exceed it
    /// fails before the call.
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
            delay is Some ==> attempt < MAX_RETRIES,
    {
        if attempt < self.retry_schedule.len() && attempt < MAX_RETRIES {
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

    /// Checks `config` and makes the bounds the agent runs under.
    ///
    /// # Errors
    /// A retry schedule longer than [`MAX_RETRIES`].
    #[verifier::external]
    pub fn parse(config: LimitsConfig) -> Result<Self, LimitsError> {
        if config.retry_schedule.len() > MAX_RETRIES {
            return Err(LimitsError::RetrySchedule {
                length: config.retry_schedule.len(),
                limit: MAX_RETRIES,
            });
        }
        Ok(Self {
            turns_per_session: config.turns_per_session.get(),
            steps_per_turn: config.steps_per_turn.get(),
            calls_per_step: config.calls_per_step.get(),
            calls_per_turn: config.calls_per_turn.get(),
            context_bytes: config.context_bytes.get(),
            retry_schedule: config.retry_schedule,
            parts_per_batch: config.parts_per_batch.get(),
            batch_window: config.batch_window,
        })
    }

    /// The bounds v0 ships with.
    ///
    /// # Panics
    /// Never: [`LimitsConfig::v0`] parses.
    #[verifier::external]
    #[must_use]
    pub fn v0() -> Self {
        Self::parse(LimitsConfig::v0()).expect("the v0 limits are consistent")
    }

    /// Group commit: model parts batched into one record before an fsync.
    ///
    /// # Panics
    /// Never: [`Limits::parse`] made it from a non-zero count.
    #[verifier::external]
    #[must_use]
    pub const fn parts_per_batch(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.parts_per_batch).expect("parsed from a NonZeroUsize")
    }

    /// The retry schedule's length: the retry budget of one step.
    #[verifier::external]
    #[must_use]
    pub const fn retries(&self) -> usize {
        self.retry_schedule.len()
    }

    /// The config these limits were parsed from.
    ///
    /// # Panics
    /// Never: every count was non-zero when parsed.
    #[verifier::external]
    #[must_use]
    pub fn config(&self) -> LimitsConfig {
        let nonzero = "parsed from a non-zero count";
        LimitsConfig {
            turns_per_session: NonZeroUsize::new(self.turns_per_session).expect(nonzero),
            steps_per_turn: NonZeroU32::new(self.steps_per_turn).expect(nonzero),
            calls_per_step: NonZeroUsize::new(self.calls_per_step).expect(nonzero),
            calls_per_turn: NonZeroUsize::new(self.calls_per_turn).expect(nonzero),
            context_bytes: NonZeroU64::new(self.context_bytes).expect(nonzero),
            retry_schedule: self.retry_schedule.clone(),
            parts_per_batch: self.parts_per_batch(),
            batch_window: self.batch_window,
        }
    }
}

/// The longest retry schedule accepted.
pub const MAX_RETRIES: usize = 64;

} // verus!

/// The raw bounds [`Limits::parse`] checks. Every count is non-zero by type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LimitsConfig {
    pub turns_per_session: NonZeroUsize,
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

/// Why a [`LimitsConfig`] is not a [`Limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitsError {
    #[error("the retry schedule has {length} entries, more than {limit}")]
    RetrySchedule { length: usize, limit: usize },
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
            turns_per_session: NonZeroUsize::new(1024).expect(nonzero),
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
    use super::{Limits, LimitsConfig, LimitsError, MAX_RETRIES, Millis};

    #[test]
    fn parse_checks_what_types_cannot() {
        let v0 = Limits::v0();
        assert_eq!(Limits::parse(v0.config()), Ok(v0), "round trip");
        let long = LimitsConfig {
            retry_schedule: vec![Millis::new(1); MAX_RETRIES + 1],
            ..LimitsConfig::v0()
        };
        assert_eq!(
            Limits::parse(long),
            Err(LimitsError::RetrySchedule {
                length: MAX_RETRIES + 1,
                limit: MAX_RETRIES
            }),
            "too many retries"
        );
        assert_eq!(Limits::v0().retry_delay(3), None, "schedule spent");
        assert_eq!(Limits::v0().retry_delay(0), Some(Millis::new(500)));
    }
}
