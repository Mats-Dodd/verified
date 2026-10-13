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

use verified_model::Replayed;
use verified_model::coverage::{Class, Key, Phase, Resolved, Situation};
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
    }
}

/// Every directed scenario.
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
                },
                Some(Hold::RanTogether),
            ),
        },
        // A `Safe` run interrupted before it acts: it reports `Interrupted`
        // and the replay counts no act (its `stopped`). Seeds 0..500 never
        // do: a read starts as soon as it is dispatched, before any
        // interrupt can cancel it.
        Directed {
            name: "a_safe_run_stopped_before_it_acts",
            target: Key::Call {
                class: Class::Safe,
                phase: Phase::Dispatched,
                crossed: false,
                resolved: Resolved::Interrupted,
            },
            scenario: one_turn(
                TurnPlan {
                    text: "read notes.txt".to_owned(),
                    verdicts: Vec::new(),
                    cancel: Some(CancelAt::ToolRunning),
                },
                Some(Hold::InterruptedFirst),
            ),
        },
        // A call past v0's 16 calls a step fails by the limit and never
        // runs. No seed proposes more than four calls in a step.
        Directed {
            name: "a_call_past_the_step_bound",
            target: Key::Call {
                class: Class::Unsafe,
                phase: Phase::Proposed,
                crossed: false,
                resolved: Resolved::Limited,
            },
            scenario: one_turn(
                TurnPlan {
                    text: format!(
                        "read notes.txt ; {}edit notes.txt alpha d16",
                        "read other.txt ; ".repeat(15)
                    ),
                    verdicts: Vec::new(),
                    cancel: None,
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
