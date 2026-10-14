//! The bounds BFS runs under: one harness per model check in
//! `tests/model.rs`, and the same harnesses for coverage ([`all`]), so the
//! gap table compares the simulation against exactly what is checked.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use verified_domain::{Limits, LimitsConfig, Millis, Pattern, Rule, Rules, Shape, Tool};

use crate::harness::{Bounded, Harness, background, plain};

/// Small limits: four turns, `steps` steps a turn, four calls a step and
/// eight a turn, one retry.
///
/// # Panics
/// `steps` is zero.
#[must_use]
pub fn config(steps: u32) -> LimitsConfig {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    LimitsConfig {
        turns_per_actor: count(4),
        messages_per_actor: count(4),
        tasks_per_actor: count(4),
        depth: 1,
        steps_per_turn: NonZeroU32::new(steps).expect("at least one step"),
        calls_per_step: count(4),
        calls_per_turn: count(8),
        context_bytes: NonZeroU64::new(1_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    }
}

/// [`config`]'s limits.
///
/// # Panics
/// `steps` is zero.
#[must_use]
pub fn limits(steps: u32) -> Limits {
    Limits::new(config(steps))
}

/// Each script a plain call of each of its tools.
fn shapes(scripts: Vec<Vec<Tool>>) -> Vec<Vec<Shape>> {
    scripts
        .into_iter()
        .map(|script| script.into_iter().map(plain).collect())
        .collect()
}

/// One actor, one message, these model responses, up to `batches` batches
/// of parts per model call, and no faults but the model's.
#[must_use]
pub fn bounded(scripts: Vec<Vec<Tool>>, batches: u8) -> Bounded {
    Bounded {
        messages: 1,
        scripts: vec![shapes(scripts)],
        batches,
        faults: true,
        stale_answers: 0,
        max_crashes: 0,
        max_interrupts: 0,
        max_duplicates: 0,
        client: None,
        slots: 1,
        roots: 1,
        actors: 1,
        steers: 0,
        stops: 0,
        max_hangs: 0,
        expiries: false,
        rules: None,
    }
}

/// A rules file that allows every call of every tool.
#[must_use]
pub fn allow_all() -> Rules {
    Rules {
        rules: [
            Tool::Read,
            Tool::Edit,
            Tool::Bash,
            Tool::Spawn,
            Tool::Send,
            Tool::Wake,
            Tool::Tasks,
            Tool::Cancel,
        ]
        .into_iter()
        .map(|tool| Rule {
            actor: None,
            tool,
            subject: Pattern::Any,
        })
        .collect(),
    }
}

/// Actors at `slots` slots, the first `roots` of them born at boot.
///
/// Each slot answers with its `scripts`; the user sends one message to the
/// first root; every call is allowed by rule; there are no model faults,
/// no batches of parts, and no other faults.
#[must_use]
pub fn actors(slots: usize, roots: usize, scripts: Vec<Vec<Vec<Shape>>>) -> Bounded {
    Bounded {
        scripts,
        batches: 0,
        faults: false,
        slots,
        roots,
        actors: slots,
        rules: Some(allow_all()),
        ..bounded(Vec::new(), 0)
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
/// Never: every count is non-zero.
#[must_use]
pub fn calls_past_the_bounds() -> Harness {
    Harness::bounded(
        Limits::new(LimitsConfig {
            calls_per_step: NonZeroUsize::new(1).expect("non-zero"),
            calls_per_turn: NonZeroUsize::new(2).expect("non-zero"),
            ..config(3)
        }),
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

/// The limits of the checks with actors: a step a turn, and spawns two
/// deep.
///
/// # Panics
/// Never: every count is non-zero.
#[must_use]
pub fn actor_limits() -> Limits {
    Limits::new(LimitsConfig {
        depth: 2,
        ..config(1)
    })
}

/// O2 and A1: an actor sends to another, or spawns it, and the daemon may
/// crash anywhere, on either side of a delivery.
#[must_use]
pub fn deliveries_across_crashes() -> Harness {
    Harness::bounded(
        actor_limits(),
        Bounded {
            max_crashes: 1,
            ..actors(
                2,
                1,
                vec![
                    vec![
                        vec![plain(Tool::Spawn)],
                        vec![plain(Tool::Spawn), plain(Tool::Send)],
                    ],
                    vec![Vec::new()],
                ],
            )
        },
    )
}

/// H1 and H3: a command in the background and a timer, each a task; the
/// daemon may crash anywhere. Two tasks and three turns an actor.
///
/// # Panics
/// Never: every count is non-zero.
#[must_use]
pub fn tasks_across_crashes() -> Harness {
    Harness::bounded(
        Limits::new(LimitsConfig {
            turns_per_actor: NonZeroUsize::new(3).expect("non-zero"),
            tasks_per_actor: NonZeroUsize::new(2).expect("non-zero"),
            ..config(2)
        }),
        Bounded {
            max_crashes: 1,
            ..actors(
                1,
                1,
                vec![vec![vec![background(), plain(Tool::Wake)], Vec::new()]],
            )
        },
    )
}

/// H1: reports race cancels, Esc and deadlines, for a call and for a task.
#[must_use]
pub fn completions_race_cancels_and_deadlines() -> Harness {
    Harness::bounded(
        limits(1),
        Bounded {
            max_interrupts: 1,
            expiries: true,
            ..actors(
                1,
                1,
                vec![vec![
                    vec![plain(Tool::Bash)],
                    vec![background(), plain(Tool::Cancel)],
                ]],
            )
        },
    )
}

/// The user's two messages, the second of which may steer a turn that
/// waits for its edit's approval.
fn steering(steps: u32, bounded: Bounded) -> Harness {
    Harness::bounded(
        limits(steps),
        Bounded {
            messages: 2,
            steers: 1,
            faults: false,
            ..bounded
        },
    )
}

/// L4 and L5: the user's messages steer a turn that waits for an approval,
/// with Esc and a crash anywhere; a step a turn, so a steered message
/// lands in the next turn.
#[must_use]
pub fn steering_cancels_and_crashes() -> Harness {
    steering(
        1,
        Bounded {
            max_interrupts: 1,
            max_crashes: 1,
            ..bounded(vec![vec![Tool::Edit], Vec::new()], 0)
        },
    )
}

/// L4 and L5: a steered message lands at the next step of its turn, or
/// after Esc, in the next turn.
#[must_use]
pub fn steering_across_steps() -> Harness {
    steering(
        2,
        Bounded {
            max_interrupts: 1,
            ..bounded(vec![vec![Tool::Edit], Vec::new()], 0)
        },
    )
}

/// B1: two actors that answer every message with one to the other.
#[must_use]
pub fn an_actor_cycle_goes_quiet() -> Harness {
    Harness::bounded(
        limits(1),
        actors(
            2,
            2,
            vec![vec![vec![plain(Tool::Send)]], vec![vec![plain(Tool::Send)]]],
        ),
    )
}

/// B2: a run, in the foreground or as a task, may go silent; its deadline
/// resolves it.
#[must_use]
pub fn hung_runs_resolve_by_their_deadline() -> Harness {
    Harness::bounded(
        limits(1),
        Bounded {
            max_hangs: 1,
            expiries: true,
            ..actors(
                1,
                1,
                vec![vec![vec![plain(Tool::Bash)], vec![background()]]],
            )
        },
    )
}

/// B3: a chain of spawns past the registry's cap on actors, and a stop of
/// the root that cascades.
#[must_use]
pub fn chatter_goes_quiet_within_limits() -> Harness {
    Harness::bounded(
        actor_limits(),
        Bounded {
            actors: 2,
            stops: 1,
            ..actors(
                3,
                1,
                vec![
                    vec![vec![plain(Tool::Spawn)]],
                    vec![vec![plain(Tool::Spawn)]],
                    vec![Vec::new()],
                ],
            )
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
        ("deliveries_across_crashes", deliveries_across_crashes()),
        ("tasks_across_crashes", tasks_across_crashes()),
        (
            "completions_race_cancels_and_deadlines",
            completions_race_cancels_and_deadlines(),
        ),
        (
            "steering_cancels_and_crashes",
            steering_cancels_and_crashes(),
        ),
        ("steering_across_steps", steering_across_steps()),
        ("an_actor_cycle_goes_quiet", an_actor_cycle_goes_quiet()),
        (
            "hung_runs_resolve_by_their_deadline",
            hung_runs_resolve_by_their_deadline(),
        ),
        (
            "chatter_goes_quiet_within_limits",
            chatter_goes_quiet_within_limits(),
        ),
    ]
}
