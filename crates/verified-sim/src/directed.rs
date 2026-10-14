//! Directed scenarios: a scenario written to reach a coverage key random
//! seeds miss.
//!
//! Each is a BFS witness to its target, written out as the simulation runs
//! it: what the user sends (each turn's text names the calls the fake model
//! proposes), how the user answers each prompt, and the one ordering no
//! seed's schedule promises ([`Scenario::hold`]): two runs acting
//! together, or an interrupt durable before a run starts. Everything else (ids, sizes, exits,
//! latencies, when records become durable) is the run's. [`run()`] fails a
//! scenario whose replay misses its target, so a scenario the model or the
//! daemon drifted away from is caught.

use core::time::Duration;
use std::collections::BTreeSet;

use core::num::{NonZeroU32, NonZeroUsize};

use verified_domain::{Limits, LimitsConfig, Unstarted};
use verified_model::Replayed;
use verified_model::coverage::{Class, Ended, Key, Phase, Resolved, Situation, Started};
use verified_server::limits::ServerLimits;
use verified_wire::command::Verdict;

use crate::hosts;
use crate::run::{self, Broken, RunError};
use crate::scenario::{CancelAt, Hold, Scenario, TurnPlan};

/// A gap random seeds miss, and the scenario that reaches it.
#[derive(Debug, Clone)]
pub struct Directed {
    pub name: &'static str,
    pub target: Key,
    pub scenario: Scenario,
}

/// A scenario of seed 0: one turn, no faults.
fn one_turn(turn: TurnPlan, hold: Option<Hold>) -> Scenario {
    Scenario {
        seed: 0,
        turns: vec![turn],
        faults: Vec::new(),
        aimed: None,
        hold,
        block_size: None,
        max_latency: Duration::from_millis(5),
        limits: hosts::limits(),
        rules: None,
        stop: false,
    }
}

/// v0's limits, with the core's changed by `change`.
fn shrunk(change: impl FnOnce(&mut LimitsConfig)) -> ServerLimits {
    let mut config = LimitsConfig::v0();
    change(&mut config);
    let mut limits = hosts::limits();
    limits.core = Limits::new(config);
    limits
}

/// Every directed scenario.
///
/// # Panics
/// Never: every limit a scenario shrinks is a non-zero constant.
#[must_use]
pub fn all() -> Vec<Directed> {
    vec![
        // Two `Unsafe` runs act before either reports. Seeds 0..500 never
        // do: a run reports as soon as it returns, so the next is
        // dispatched only after the shell took the report.
        Directed {
            name: "two_unsafe_runs_act_at_once",
            target: Key::Situation(Situation::UnsafeTogether),
            scenario: one_turn(
                TurnPlan {
                    text: "edit notes.txt alpha d0 ; bash touch d1".to_owned(),
                    verdicts: vec![Verdict::Grant, Verdict::Grant],
                    cancel: None,
                    steer: None,
                },
                Some(Hold::RanTogether),
            ),
        },
        // A `Safe` run interrupted before it acts: it reports `NotStarted`
        // and the replay counts no act (its `stopped`). Seeds 0..500 never
        // do: a read starts as soon as it is dispatched, before any
        // interrupt can cancel it.
        Directed {
            name: "a_safe_run_stopped_before_it_acts",
            target: Key::Call {
                class: Class::Safe,
                phase: Phase::Dispatched,
                crossed: false,
                resolved: Resolved::NotStarted(Unstarted::Cancelled),
            },
            scenario: one_turn(
                TurnPlan {
                    text: "read notes.txt".to_owned(),
                    verdicts: Vec::new(),
                    cancel: Some(CancelAt::ToolRunning),
                    steer: None,
                },
                Some(Hold::CancelledFirst),
            ),
        },
        // A turn whose script runs past the step limit (4 here) fails by
        // it. No seed's script runs more than three steps.
        Directed {
            name: "a_turn_past_its_step_limit",
            target: Key::Turn(Ended::Steps),
            scenario: Scenario {
                limits: shrunk(|config| {
                    config.steps_per_turn = NonZeroU32::new(4).expect("non-zero");
                }),
                ..one_turn(
                    TurnPlan::granted(&["read notes.txt"; 6].join(" then ")),
                    None,
                )
            },
        },
        // A background command that fails: its task ends `Failed`. No
        // seed's background command fails.
        Directed {
            name: "a_background_command_that_fails",
            target: Key::Task {
                started: Started::Call,
                resolved: Resolved::Failed,
            },
            scenario: one_turn(TurnPlan::granted("bg false"), None),
        },
        // A send to a child whose mailbox is full (two messages here) is
        // refused by the limit: it never lands. No seed sends a child more
        // than two messages.
        Directed {
            name: "a_send_past_its_recipients_limit",
            target: Key::Call {
                class: Class::Keyed,
                phase: Phase::Dispatched,
                crossed: false,
                resolved: Resolved::NotStarted(Unstarted::Limit),
            },
            scenario: Scenario {
                limits: shrunk(|config| {
                    config.messages_per_actor = NonZeroUsize::new(2).expect("non-zero");
                }),
                ..Scenario::of(vec![TurnPlan::granted(
                    "spawn helper hello then send $child a then send $child b",
                )])
            },
        },
        // A background command past the actor's task limit (one here)
        // does not start, though approved; nor does a timer.
        Directed {
            name: "a_command_past_the_task_limit",
            target: Key::Call {
                class: Class::Unsafe,
                phase: Phase::Approved,
                crossed: false,
                resolved: Resolved::NotStarted(Unstarted::Limit),
            },
            scenario: Scenario {
                limits: shrunk(|config| {
                    config.tasks_per_actor = NonZeroUsize::new(1).expect("non-zero");
                }),
                ..one_turn(TurnPlan::granted("bg sleep 300 ; bg sleep 300"), None)
            },
        },
        Directed {
            name: "a_timer_past_the_task_limit",
            target: Key::Call {
                class: Class::Safe,
                phase: Phase::Approved,
                crossed: false,
                resolved: Resolved::NotStarted(Unstarted::Limit),
            },
            scenario: Scenario {
                limits: shrunk(|config| {
                    config.tasks_per_actor = NonZeroUsize::new(1).expect("non-zero");
                }),
                ..Scenario::of(vec![TurnPlan::granted("wake 300 ; wake 300")])
            },
        },
        // A read past v0's 16 calls a step fails by the limit and never
        // runs.
        Directed {
            name: "a_read_past_the_step_bound",
            target: Key::Call {
                class: Class::Safe,
                phase: Phase::Proposed,
                crossed: false,
                resolved: Resolved::NotStarted(Unstarted::Limit),
            },
            scenario: one_turn(TurnPlan::granted(&["read other.txt"; 17].join(" ; ")), None),
        },
        // A call past v0's 16 calls a step fails by the limit and never
        // runs. No seed proposes more than four calls in a step.
        Directed {
            name: "a_call_past_the_step_bound",
            target: Key::Call {
                class: Class::Unsafe,
                phase: Phase::Proposed,
                crossed: false,
                resolved: Resolved::NotStarted(Unstarted::Limit),
            },
            scenario: one_turn(
                TurnPlan {
                    text: format!(
                        "read notes.txt ; {}edit notes.txt alpha d16",
                        "read other.txt ; ".repeat(15)
                    ),
                    verdicts: Vec::new(),
                    cancel: None,
                    steer: None,
                },
                None,
            ),
        },
    ]
}

/// The directed scenario called `name`.
#[must_use]
pub fn named(name: &str) -> Option<Directed> {
    all().into_iter().find(|directed| directed.name == name)
}

/// Why a directed scenario failed.
#[derive(Debug, thiserror::Error)]
pub enum DirectedError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Broken(#[from] Broken),
    #[error("the directed scenario {name} missed its target {target}; it reached {reached:?}")]
    Missed {
        name: &'static str,
        target: Key,
        reached: BTreeSet<Key>,
    },
}

/// Runs `directed`'s scenario twice and judges it.
///
/// # Errors
/// A run failed or diverged from the other, it broke an invariant (the
/// model's replay among them), or its replay missed the target.
pub fn run(directed: &Directed) -> Result<Replayed, DirectedError> {
    let replayed = run::twice(&directed.scenario)?.verdict()?;
    if replayed.keys.contains(&directed.target) {
        Ok(replayed)
    } else {
        Err(DirectedError::Missed {
            name: directed.name,
            target: directed.target,
            reached: replayed.keys,
        })
    }
}
