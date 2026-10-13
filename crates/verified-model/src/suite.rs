//! The bounds BFS runs under: one harness per model check in
//! `tests/model.rs`, and the same harnesses for coverage ([`all`]), so the
//! gap table compares the simulation against exactly what is checked.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use verified_domain::{Limits, LimitsConfig, Millis, Tool};

use crate::harness::{Bounded, Harness};

/// Small limits: four turns, `steps` steps a turn, four calls a step and
/// eight a turn, one retry.
///
/// # Panics
/// `steps` is zero.
#[must_use]
pub fn config(steps: u32) -> LimitsConfig {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    LimitsConfig {
        turns_per_session: count(4),
        steps_per_turn: NonZeroU32::new(steps).expect("at least one step"),
        calls_per_step: count(4),
        calls_per_turn: count(8),
        context_bytes: NonZeroU64::new(1_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    }
}

/// [`config`], parsed.
///
/// # Panics
/// `steps` is zero.
#[must_use]
pub fn limits(steps: u32) -> Limits {
    Limits::parse(config(steps)).expect("consistent limits")
}

/// One message, these model responses, up to `batches` batches of parts
/// per model call, and no faults.
#[must_use]
pub const fn bounded(scripts: Vec<Vec<Tool>>, batches: u8) -> Bounded {
    Bounded {
        messages: 1,
        scripts,
        batches,
        stale_answers: 0,
        max_crashes: 0,
        max_interrupts: 0,
        max_duplicates: 0,
        client: None,
    }
}

/// A crash anywhere, including mid-persist, with these scripts, and one
/// batch of parts per model call (a second multiplies the space by up to
/// ten; the checks that need it ask for it).
#[must_use]
pub fn crashing(scripts: Vec<Vec<Tool>>) -> Bounded {
    Bounded {
        max_crashes: 1,
        ..bounded(scripts, 1)
    }
}

/// Every fault except crashes: interrupts and late duplicates.
#[must_use]
pub fn without_crashes() -> Harness {
    Harness::bounded(
        limits(2),
        Bounded {
            max_interrupts: 1,
            max_duplicates: 1,
            ..bounded(vec![Vec::new(), vec![Tool::Read], vec![Tool::Bash]], 2)
        },
    )
}

/// [`crashing`] with these scripts, under `steps` steps a turn.
#[must_use]
pub fn with_crashes(scripts: Vec<Vec<Tool>>, steps: u32) -> Harness {
    Harness::bounded(limits(steps), crashing(scripts))
}

/// Crashes with a tool of every class, two batches, stale answers.
///
/// The user may also answer one call not awaiting an answer (repeated,
/// late, or after a crash), as a real client does. That roughly doubles a
/// space, so only the checks with crashes and approvals ask for it.
#[must_use]
pub fn crashes_with_every_tool_class() -> Harness {
    Harness::bounded(
        limits(1),
        Bounded {
            batches: 2,
            stale_answers: 1,
            ..crashing(vec![
                Vec::new(),
                vec![Tool::Bash],
                vec![Tool::Edit, Tool::Read],
            ])
        },
    )
}

/// Crashes across two steps of a turn.
#[must_use]
pub fn crashes_across_steps() -> Harness {
    with_crashes(vec![Vec::new(), vec![Tool::Edit]], 2)
}

/// Crashes and an interrupt, two batches, stale answers.
#[must_use]
pub fn crashes_while_cancelling() -> Harness {
    Harness::bounded(
        limits(1),
        Bounded {
            batches: 2,
            stale_answers: 1,
            max_interrupts: 1,
            ..crashing(vec![Vec::new(), vec![Tool::Bash], vec![Tool::Edit]])
        },
    )
}

/// One call a step and two a turn, so a model may propose past them.
///
/// # Panics
/// Never: the limits are consistent.
#[must_use]
pub fn calls_past_the_bounds() -> Harness {
    Harness::bounded(
        Limits::parse(LimitsConfig {
            calls_per_step: NonZeroUsize::new(1).expect("non-zero"),
            calls_per_turn: NonZeroUsize::new(2).expect("non-zero"),
            ..config(3)
        })
        .expect("consistent limits"),
        crashing(vec![Vec::new(), vec![Tool::Read, Tool::Edit]]),
    )
}

/// An edit, a command and a read in one step.
#[must_use]
pub fn unsafe_calls_run_together() -> Harness {
    with_crashes(vec![vec![Tool::Edit, Tool::Bash, Tool::Read]], 1)
}

/// A client follows the stream and may reconnect once.
#[must_use]
pub fn a_client_follows_the_stream() -> Harness {
    Harness::bounded(
        limits(1),
        Bounded {
            client: Some(1),
            ..crashing(vec![Vec::new(), vec![Tool::Read]])
        },
    )
}

/// Every bound BFS checks the model under, by its check's name.
#[must_use]
pub fn all() -> Vec<(&'static str, Harness)> {
    vec![
        ("interrupts_and_duplicates", without_crashes()),
        (
            "crashes_with_every_tool_class",
            crashes_with_every_tool_class(),
        ),
        ("crashes_across_steps", crashes_across_steps()),
        ("crashes_while_cancelling", crashes_while_cancelling()),
        ("calls_past_the_bounds", calls_past_the_bounds()),
        ("unsafe_calls_run_together", unsafe_calls_run_together()),
        ("a_client_follows_the_stream", a_client_follows_the_stream()),
    ]
}
