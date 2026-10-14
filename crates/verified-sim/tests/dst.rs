//! Deterministic simulation: the daemon, the TUI and a fake Anthropic on
//! turmoil, with crashes, partitions and torn writes drawn from each seed.
//!
//! Every seed runs twice and the two traces must match before any invariant
//! is judged: a check over a nondeterministic run proves nothing.
//!
//! Run a failing seed again with `cargo run -p verified-sim -- run --from N
//! --count 1`, and see its trace with `cargo run -p verified-sim -- trace
//! --seed N`. Two runs in one process cannot see randomness drawn once per
//! process; `just sim` also runs `verified-sim processes`, which traces each
//! fixed seed in two fresh processes.

use core::time::Duration;
use std::collections::BTreeMap;

use verified_domain::{Exit, ModelFault, Record, Reset, TurnOutcome, Unstarted};
use verified_model::coverage::{Class, Key, Phase, Resolved};
use verified_model::{Bug, Harness, Kind, Mismatch, Observed};
use verified_server::limits::ServerLimits;
use verified_server::shell::Planted;
use verified_server::shim::Observation;
use verified_sim::directed::{self, Directed, DirectedError};
use verified_sim::ledger::{Entry, Op};
use verified_sim::run::Outcome;
use verified_sim::scenario::{Fault, Scenario};
use verified_sim::{hosts, model, run};

/// Runs `seed` twice, compares the traces, then judges the run.
fn seed(seed: u64) {
    run::twice(&Scenario::from_seed(seed))
        .map_err(|error| {
            let tail = error
                .trace()
                .iter()
                .rev()
                .take(40)
                .rev()
                .collect::<Vec<_>>();
            format!("{error}\nlast trace lines:\n{tail:#?}")
        })
        .expect("the run finishes, deterministically")
        .verdict()
        .map_err(|broken| broken.to_string())
        .expect("the run breaks nothing");
}

/// One run of `seed`, with the defect `planted` in the daemon.
fn once(seed: u64, planted: Planted) -> Outcome {
    run::once(&Scenario::from_seed(seed), planted)
        .map_err(|error| error.to_string())
        .expect("the run finishes")
}

/// Different seeds must steer the world differently, or the seed is not
/// reaching it.
#[test]
fn seeds_steer_the_world() {
    let first = once(1, Planted::None);
    let second = once(2, Planted::None);
    assert_ne!(
        first.trace(),
        second.trace(),
        "two seeds made the same world"
    );
}

macro_rules! seeds {
    ($($name:ident = $seed:expr;)*) => {
        $(
            #[test]
            fn $name() {
                seed($seed);
            }
        )*
    };
}

seeds! {
    seed_0 = 0;
    seed_1 = 1;
    seed_2 = 2;
    seed_3 = 3;
    seed_4 = 4;
    seed_5 = 5;
    seed_6 = 6;
    seed_7 = 7;
    // The daemon crashes while a command runs: it resolves Unknown, and a
    // core that reran it would be caught here by the replay.
    seed_37 = 37;
    // A reconnect resumes mid-turn: only the read-by-read I6 catches a
    // follower that skips a record there.
    seed_50 = 50;
    // The daemon is down from the start: the TUI's create gives up and the
    // user retries it by hand.
    seed_51 = 51;
    // The daemon crashes after an edit is dispatched and before it writes:
    // the edit resolves Unknown. Later two edits and a command are proposed
    // in one step.
    seed_53 = 53;
    // The daemon crashes after a command is dispatched, so it resolves
    // Unknown; then the TUI link stays down past the follower's reconnect
    // schedule, so it stops following and the user reconnects by hand.
    seed_818 = 818;
    // A tool runs while the next decision is being persisted: the model's
    // world goes on during a sync, and only a run seen doing so pins that.
    seed_342 = 342;
}

/// Whether the scripted user did `what`.
fn user(outcome: &Outcome, what: &str) -> bool {
    outcome
        .entries
        .iter()
        .any(|stamped| matches!(&stamped.entry, Entry::User(done) if done == what))
}

/// Whether the harness injected a fault described so.
fn fault(outcome: &Outcome, described: &str) -> bool {
    outcome.entries.iter().any(
        |stamped| matches!(&stamped.entry, Entry::Fault(fault) if fault.starts_with(described)),
    )
}

/// Whether the replay reached each of `keys`.
fn reached(outcome: &Outcome, keys: &[Key]) -> bool {
    outcome
        .replayed
        .as_ref()
        .is_ok_and(|replayed| keys.iter().all(|key| replayed.keys.contains(key)))
}

/// Whether a group commit made durable a step reset to retry a model call
/// that failed over the network.
fn retried_over_the_network(outcome: &Outcome) -> bool {
    outcome.entries.iter().any(|stamped| {
        matches!(&stamped.entry, Entry::Acked { records, .. } if records.iter().any(|record| {
            matches!(
                record,
                Record::StepReset {
                    reason: Reset::Retry {
                        fault: ModelFault::Network,
                        ..
                    },
                    ..
                }
            )
        }))
    })
}

/// Whether a command and two edits that one model call proposed all acted
/// on the world (the fake model's ids name the turn, then the position).
fn command_and_two_edits_act(outcome: &Outcome) -> bool {
    let mut acts: BTreeMap<&str, (bool, usize)> = BTreeMap::new();
    for stamped in &outcome.entries {
        if let Entry::Mutation {
            call: Some(call),
            op,
            ..
        } = &stamped.entry
        {
            let turn = call
                .rsplit_once('_')
                .map_or(call.as_str(), |(turn, _)| turn);
            let (command, edits) = acts.entry(turn).or_default();
            match op {
                Op::Exec { .. } => *command = true,
                Op::Write { .. } => *edits = edits.saturating_add(1),
            }
        }
    }
    acts.values()
        .any(|(command, edits)| *command && *edits >= 2)
}

/// Whether a run reached a path.
type Reaches = fn(&Outcome) -> bool;

#[test]
fn the_fixed_seeds_reach_every_fault_path() {
    // Each path, and whether a run reached it: a path nobody reaches is a
    // path nobody checks.
    let paths: [(&str, Reaches); 6] = [
        (
            "the TUI stops following (Down) and the user reconnects",
            |outcome| user(outcome, "reconnect"),
        ),
        (
            "the TUI's create gives up and the user retries it",
            |outcome| user(outcome, "retry creating"),
        ),
        (
            "a daemon-Anthropic partition fails a model call over the network, and the core retries it",
            |outcome| {
                fault(outcome, "the model link partitions") && retried_over_the_network(outcome)
            },
        ),
        (
            "the daemon crashes after an Unsafe call is dispatched and before it acts, and the call resolves Unknown",
            |outcome| {
                reached(
                    outcome,
                    &[
                        Key::Crashed {
                            class: Class::Unsafe,
                            ran: false,
                        },
                        Key::Call {
                            class: Class::Unsafe,
                            phase: Phase::Dispatched,
                            crossed: true,
                            resolved: Resolved::Unknown,
                        },
                    ],
                )
            },
        ),
        (
            "the daemon crashes while a command runs, after it acted, and the command resolves Unknown",
            |outcome| {
                reached(
                    outcome,
                    &[
                        Key::Crashed {
                            class: Class::Unsafe,
                            ran: true,
                        },
                        Key::Call {
                            class: Class::Unsafe,
                            phase: Phase::Ran,
                            crossed: true,
                            resolved: Resolved::Unknown,
                        },
                    ],
                )
            },
        ),
        (
            "two edits and a command proposed in one step all act",
            command_and_two_edits_act,
        ),
    ];
    let outcomes: Vec<Outcome> = run::FIXED
        .iter()
        .map(|seed| once(*seed, Planted::None))
        .collect();
    for (path, reaches) in paths {
        assert!(
            outcomes.iter().any(reaches),
            "no fixed seed reaches: {path}"
        );
    }
}

/// Persist before acting (Law 2), as a mutation test: a daemon that starts
/// tool runs before their decision is durable leaves the model on some
/// seed, where the shell dispatches (or a tool acts for) a run the model has
/// not started, because its decision is not durable yet.
#[test]
fn running_tools_before_their_dispatch_is_durable_fails_the_replay() {
    let caught = (0..16u64).any(|seed| {
        matches!(
            once(seed, Planted::RunBeforePersist).replayed,
            Err(Mismatch::Diverged(divergence))
                if matches!(divergence.observed, Observed::Dispatched(_) | Observed::Mutated(_))
        )
    });
    assert!(caught, "no seed's replay caught a tool run before persist");
}

/// At most once inside one dispatch, as a mutation test: a shell that runs
/// a tool twice and reports it once is caught by the replay at the second
/// act on the world, though the shell's own `ToolRan` is passed only once.
#[test]
fn running_a_tool_twice_in_one_dispatch_fails_the_replay() {
    let caught = (0..16u64).any(|seed| {
        let outcome = once(seed, Planted::RunTwice);
        let Err(Mismatch::Diverged(divergence)) = &outcome.replayed else {
            return false;
        };
        let Observed::Mutated(id) = divergence.observed else {
            return false;
        };
        outcome
            .observed
            .get(..divergence.index)
            .is_some_and(|before| before.contains(&Observed::Mutated(id)))
    });
    assert!(caught, "no seed's replay caught a second act");
}

/// The model's `Execute` is the run acting on the world, not its return:
/// seed 37's command starts (a mutation) and the daemon crashes before it
/// returns, so the crash comes after the `Unsafe` run acted.
#[test]
fn a_crash_while_a_command_runs_is_after_it_acted() {
    let outcome = once(37, Planted::None);
    assert!(
        reached(
            &outcome,
            &[Key::Crashed {
                class: Class::Unsafe,
                ran: true,
            }]
        ),
        "seed 37 crashes after its command acted: {:?}",
        outcome.replayed
    );
}

/// What the shell makes durable is what the core decided, as a mutation
/// test: a shell that persists an `Unknown` result as `NotStarted` (H2,
/// broken below the core) leaves the model where a group commit becomes
/// durable holding other records than the model's.
#[test]
fn persisting_unknown_as_not_started_fails_the_replay() {
    let outcome = once(37, Planted::UnknownAsNotStarted);
    let interrupted = |record: &Record<_>| {
        matches!(
            record,
            Record::ToolResult {
                exit: Exit::NotStarted(Unstarted::Cancelled),
                ..
            }
        )
    };
    assert!(
        matches!(
            &outcome.replayed,
            Err(Mismatch::Diverged(divergence))
                if matches!(&divergence.observed, Observed::Acked { records, .. } if records.iter().any(interrupted))
        ),
        "seed 37 (a command resolves Unknown) diverges at its commit: {:?}",
        outcome.replayed
    );
}

/// H2, as a mutation test: a shell whose stopped command reports
/// `NotStarted`, as `Interrupted` did, claims a run that acted never began.
/// The replay refuses the report: the model admits `NotStarted` only from a
/// cancelled run that never acted.
#[test]
fn reporting_a_command_stopped_mid_run_as_not_started_fails_the_replay() {
    let caught = (0..16u64).any(|seed| {
        let outcome = once(seed, Planted::StoppedAsNotStarted);
        let Err(Mismatch::Diverged(divergence)) = &outcome.replayed else {
            return false;
        };
        matches!(
            &divergence.observed,
            Observed::Decided(_, records) if records.iter().any(|record| matches!(
                record,
                Record::ToolResult {
                    exit: Exit::NotStarted(_),
                    output: Some(_),
                    ..
                }
            ))
        )
    });
    assert!(
        caught,
        "no seed's replay caught a stopped command reported as never started"
    );
}

/// The model's properties are checked on every state a DST run passes
/// through: replayed through a model whose shell reruns `Unsafe` calls
/// after a crash, seed 37 (the daemon crashes while a command runs)
/// breaks at-most-once, though the same run replays cleanly through the
/// true model.
#[test]
fn a_property_breaks_on_a_dst_run_through_a_planted_model() {
    let outcome = once(37, Planted::None);
    assert!(
        outcome.replayed.is_ok(),
        "seed 37 replays: {:?}",
        outcome.replayed
    );
    let harness = Harness {
        bug: Some(Bug::RerunUnsafe),
        ..model::harness(hosts::limits().core, &outcome.observed)
    };
    let broken = match verified_model::replay(&harness, &outcome.observed) {
        Err(Mismatch::Violated { property, .. }) => Some(property),
        Ok(_) | Err(Mismatch::Diverged(_)) => None,
    };
    assert_eq!(
        broken,
        Some("an Unsafe call is dispatched at most once across crashes"),
        "the rerun is caught as a broken property"
    );
}

/// The replay is not vacuous: across the fixed seeds it takes every kind of
/// step DST can make the model take.
#[test]
fn the_fixed_seeds_replay_every_kind_of_step() {
    let mut taken = std::collections::BTreeSet::new();
    for seed in run::FIXED {
        let replayed = once(seed, Planted::None)
            .verdict()
            .map_err(|broken| broken.to_string())
            .expect("the run breaks nothing");
        taken.extend(replayed.keys);
    }
    for kind in [
        Kind::Send,
        Kind::Answer,
        Kind::Interrupt,
        Kind::Execute,
        Kind::Parts,
        Kind::Finish,
        Kind::Fail,
        Kind::ToolDone,
        Kind::Sync,
        Kind::Crash,
        Kind::Read,
        Kind::Reconnect,
    ] {
        assert!(
            taken.contains(&Key::Action(kind)),
            "no fixed seed replays a {kind:?}: {taken:?}"
        );
    }
}

/// v0's limits, shrunk so a seed's few turns fill the log: one line and
/// the reserve that ends a turn, no more.
fn a_small_log() -> ServerLimits {
    let mut limits = hosts::limits();
    limits.wire.max_message_text_bytes = 1_000;
    limits.tool_output_bytes = 1_000;
    limits.log_line_bytes = 16usize << 10u32;
    limits.log_file_bytes = u64::try_from(limits.log_line_bytes)
        .expect("small")
        .saturating_add(limits.reserve());
    limits
}

/// Seeds whose turns fill [`a_small_log`] while a turn runs, and the
/// simulated millisecond of a crash added as it fills (seed 28's leaves
/// a resume the full log refuses).
const FILLING: [(u64, Option<u64>); 4] = [(28, None), (53, None), (77, None), (28, Some(37_424))];

/// A full log ends the running turn within its reserve, refuses what
/// follows, and the run is still a path of the model: the refused
/// decisions never reached the core, a refused resume is decided again
/// once the turn is interrupted, and the turn ends cancelled.
#[test]
fn a_full_log_ends_the_running_turn_and_replays() {
    for (seed, crash) in FILLING {
        let mut scenario = Scenario {
            limits: a_small_log(),
            ..Scenario::from_seed(seed)
        };
        if let Some(at) = crash {
            scenario.faults.push((
                Duration::from_millis(at),
                Fault::Crash {
                    down: Duration::from_millis(300),
                },
            ));
            scenario.faults.sort_by_key(|(at, _)| *at);
        }
        let outcome = run::twice(&scenario)
            .map_err(|error| error.to_string())
            .expect("the run finishes, deterministically");
        let filled = outcome
            .entries
            .iter()
            .position(|stamped| stamped.entry == Entry::Shell(Observation::Refused))
            .expect("the log fills");
        let cancelled = outcome.entries.iter().skip(filled).any(|stamped| {
            matches!(&stamped.entry, Entry::Acked { records, .. } if records.iter().any(|record| {
                matches!(
                    record,
                    Record::TurnEnded {
                        outcome: TurnOutcome::Cancelled,
                        ..
                    }
                )
            }))
        });
        assert!(
            cancelled,
            "seed {seed}: the turn running as the log fills ends cancelled"
        );
        assert_eq!(
            outcome.observed.contains(&Observed::Refused),
            crash.is_some(),
            "seed {seed}: only the crash leaves a resume refused"
        );
        outcome
            .verdict()
            .map_err(|broken| broken.to_string())
            .expect("a small log breaks nothing");
    }
}

/// Each directed scenario reaches its target, deterministically and
/// without breaking an invariant: the gaps no seed in 0..500 reaches
/// (`just coverage 0 500`).
#[test]
fn directed_scenarios_reach_their_targets() {
    for directed in directed::all() {
        directed::run(&directed)
            .map_err(|error| error.to_string())
            .expect("the directed scenario reaches its target");
    }
}

/// Each hold is the scenario's essential ordering, not decoration: without
/// it the same turn runs its two `Unsafe` calls one after the other, and
/// its read starts before the interrupt can stop it.
#[test]
fn without_its_hold_a_directed_scenario_misses_its_target() {
    for directed in directed::all() {
        if directed.scenario.hold.is_none() {
            continue;
        }
        let name = directed.name;
        let apart = Directed {
            scenario: Scenario {
                hold: None,
                ..directed.scenario
            },
            ..directed
        };
        assert!(
            matches!(directed::run(&apart), Err(DirectedError::Missed { .. })),
            "{name} reaches its target without its hold, so the hold proves nothing"
        );
    }
}
