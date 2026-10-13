//! Directed scenarios: a path BFS found to a coverage key, compiled into a
//! scenario the simulation runs.
//!
//! A witness is a total order of the model's steps. A run cannot be
//! scheduled that tightly, and need not be: the compiler keeps only what
//! makes the target happen. That is what the user sends (each turn's text
//! names the calls the fake model proposes), how the user answers each
//! prompt, and the orderings no seed's schedule promises, as [`Hold`]s.
//! Everything else (ids, sizes, exits, latencies, when records become
//! durable) is the run's. A step with no compiled form is refused, never
//! dropped, and a directed scenario's test asserts that its replay reached
//! the target: one that misses fails.

use core::time::Duration;

use stateright::Model as _;
use verified_domain::{Abstract, Proposal, Request, Tool};
use verified_model::coverage::{Class, Key, Phase, Resolved, Situation, explore};
use verified_model::{Action, Harness, Job, World, suite};
use verified_wire::command::Verdict;

use crate::hosts;
use crate::scenario::{Hold, Scenario, TurnPlan, WORDS};

/// A gap random seeds miss, the bounds BFS finds a path to it under, and
/// the seed that draws whatever the scenario leaves open.
#[derive(Debug, Clone)]
pub struct Directed {
    pub name: &'static str,
    pub harness: Harness,
    pub target: Key,
    pub seed: u64,
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
            harness: Harness {
                max_crashes: 0,
                ..suite::with_crashes(vec![vec![Tool::Edit, Tool::Bash]], 1)
            },
            target: Key::Situation(Situation::UnsafeTogether),
            seed: 7,
        },
        // A call past v0's `calls_per_step` fails by the limit and never
        // runs. No seed proposes more than four calls in a step.
        Directed {
            name: "a_call_past_the_step_bound",
            harness: Harness {
                max_crashes: 0,
                ..suite::calls_past_the_bounds()
            },
            target: Key::Call {
                class: Class::Unsafe,
                phase: Phase::Proposed,
                crossed: false,
                resolved: Resolved::Limited,
            },
            seed: 11,
        },
    ]
}

/// Why a witness has no scenario.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CompileError {
    #[error("BFS under the directed bounds never reaches {0}")]
    Unreached(Key),
    #[error("step {index} of the witness, {action:?}, has no compiled form: {why}")]
    Unsupported {
        index: usize,
        action: Action,
        why: &'static str,
    },
    #[error("step {index} of the witness, {action:?}, is not a step of the model")]
    NotAStep { index: usize, action: Action },
}

/// The shortest path BFS finds to `directed`'s target under its bounds.
///
/// # Errors
/// BFS never reaches the target.
pub fn witness(directed: &Directed) -> Result<Vec<Action>, CompileError> {
    explore(&directed.harness)
        .witnesses
        .remove(&directed.target)
        .ok_or(CompileError::Unreached(directed.target))
}

/// The command the fake model reads as `proposal`, the `position`th call.
fn command(proposal: &Proposal<Abstract>, position: usize) -> String {
    let word = WORDS
        .get(position.checked_rem(WORDS.len()).unwrap_or(0))
        .copied()
        .unwrap_or("alpha");
    match &proposal.request {
        Request::Run { args: Tool::Read } => "read notes.txt".to_owned(),
        Request::Run { args: Tool::Edit } => format!("edit notes.txt {word} d{position}"),
        Request::Run { args: Tool::Bash } => format!("bash touch d{position}"),
        Request::Invalid { .. } => "call nope {}".to_owned(),
    }
}

/// The text a user sends for the fake model to propose `calls`. The calls
/// within the witness's `bound` on calls a step are padded with reads to
/// the run's own bound, so those past it stay past it.
fn text(calls: &[Proposal<Abstract>], bound: usize, turn: usize) -> String {
    if calls.is_empty() {
        return format!("hello {turn}");
    }
    let run_bound = hosts::limits().core.calls_per_step();
    let (within, past) = calls.split_at(calls.len().min(bound));
    let mut commands: Vec<String> = within
        .iter()
        .enumerate()
        .map(|(position, call)| command(call, position))
        .collect();
    if !past.is_empty() {
        commands.resize(run_bound.max(commands.len()), "read other.txt".to_owned());
    }
    let first = commands.len();
    commands.extend(
        past.iter()
            .enumerate()
            .map(|(offset, call)| command(call, first.saturating_add(offset))),
    );
    commands.join(" ; ")
}

/// Another tool run in `world` that has acted and not reported.
fn another_acted(world: &World, id: verified_domain::EffectId) -> bool {
    world.flights.iter().any(|(other, flight)| {
        *other != id && flight.executed && matches!(flight.job, Job::Tool { .. })
    })
}

/// A turn being compiled: its plan, and its prompts' answers by the
/// proposal they answer (the TUI prompts in proposal order).
struct Turn {
    plan: TurnPlan,
    proposed: Vec<verified_domain::CallId>,
    answers: Vec<(usize, Verdict)>,
}

impl Turn {
    fn finish(mut self) -> TurnPlan {
        self.answers.sort_by_key(|(position, _)| *position);
        self.plan.verdicts = self
            .answers
            .into_iter()
            .map(|(_, verdict)| verdict)
            .collect();
        self.plan
    }
}

/// `witness`, a path through `harness`, as a scenario of `seed`.
///
/// # Errors
/// A step is not one the model takes there, or has no compiled form: a
/// crash, an interrupt, a model failure, a stale or late answer, tools
/// proposed past a turn's first step, or anything only the model's world
/// does (orphans, duplicates).
///
/// # Panics
/// Never: the model boots into one state.
pub fn compile(harness: &Harness, witness: &[Action], seed: u64) -> Result<Scenario, CompileError> {
    let unsupported = |index: usize, action: &Action, why: &'static str| {
        Err(CompileError::Unsupported {
            index,
            action: action.clone(),
            why,
        })
    };
    let bound = harness.limits.calls_per_step();
    let mut world = harness
        .init_states()
        .into_iter()
        .next()
        .expect("the model boots into one state");
    let mut turns: Vec<TurnPlan> = Vec::new();
    let mut turn: Option<Turn> = None;
    let mut holds = Vec::new();
    for (index, action) in witness.iter().enumerate() {
        match action {
            Action::Send(_) => {
                turns.extend(turn.take().map(Turn::finish));
                turn = Some(Turn {
                    plan: TurnPlan {
                        text: format!("hello {}", turns.len()),
                        verdicts: Vec::new(),
                        cancel: None,
                    },
                    proposed: Vec::new(),
                    answers: Vec::new(),
                });
            }
            Action::Finish(_, calls) => {
                let Some(turn) = turn.as_mut() else {
                    return unsupported(index, action, "a model call outside a turn");
                };
                if !calls.is_empty() {
                    if !turn.proposed.is_empty() {
                        return unsupported(
                            index,
                            action,
                            "the fake model proposes tools only in a turn's first step",
                        );
                    }
                    turn.plan.text = text(calls, bound, turns.len());
                    turn.proposed = calls.iter().map(|call| call.id.clone()).collect();
                }
            }
            Action::Answer(call, verdict) => {
                let Some(turn) = turn.as_mut() else {
                    return unsupported(index, action, "an answer outside a turn");
                };
                let position = turn.proposed.iter().position(|id| id == call);
                let (true, Some(position)) = (world.asks.contains(call), position) else {
                    return unsupported(
                        index,
                        action,
                        "the scripted user answers only the prompt shown",
                    );
                };
                turn.answers.push((
                    position,
                    match verdict {
                        verified_core::Verdict::Approve => Verdict::Grant,
                        verified_core::Verdict::Deny => Verdict::Deny,
                    },
                ));
            }
            Action::Execute(id) => {
                if another_acted(&world, *id) && !holds.contains(&Hold::RanTogether) {
                    holds.push(Hold::RanTogether);
                }
            }
            // The run's own schedule, sizes and exits.
            Action::Parts(..)
            | Action::ToolDone(..)
            | Action::Woke(_)
            | Action::Sync
            | Action::Read
            | Action::Reconnect => {}
            Action::Interrupt(_) => {
                return unsupported(index, action, "no hold places an interrupt yet");
            }
            Action::Fail(..) => {
                return unsupported(index, action, "no hold places a model failure yet");
            }
            Action::Crash(_) => {
                return unsupported(index, action, "no hold places a crash yet");
            }
            Action::OrphanExecute(_) | Action::OrphanComplete(_) | Action::Duplicate => {
                return unsupported(index, action, "only the model's world does it");
            }
        }
        world =
            harness
                .next_state(&world, action.clone())
                .ok_or_else(|| CompileError::NotAStep {
                    index,
                    action: action.clone(),
                })?;
    }
    turns.extend(turn.map(Turn::finish));
    Ok(Scenario {
        seed,
        turns,
        faults: Vec::new(),
        aimed: None,
        holds,
        block_size: None,
        max_latency: Duration::from_millis(5),
        limits: hosts::limits(),
    })
}

/// `directed`'s scenario: its witness, compiled.
///
/// # Errors
/// As [`witness`] and [`compile`].
pub fn scenario(directed: &Directed) -> Result<Scenario, CompileError> {
    compile(&directed.harness, &witness(directed)?, directed.seed)
}
