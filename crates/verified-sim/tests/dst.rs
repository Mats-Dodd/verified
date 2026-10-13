//! Deterministic simulation: the daemon, the TUI and a fake Anthropic on
//! turmoil, with crashes, partitions and torn writes drawn from each seed.
//!
//! Every seed runs twice and the two traces must match before any invariant
//! is judged: a check over a nondeterministic run proves nothing.
//!
//! Replay a failing seed with `cargo run -p verified-sim -- replay --seed N --trace`.
//! Two runs in one process cannot see randomness drawn once per process;
//! `just sim` also runs `verified-sim processes`, which traces each fixed
//! seed in two fresh processes.

use core::time::Duration;

use verified_model::coverage::{Class, Key};
use verified_model::replay::harness;
use verified_model::{Bug, Harness, Mismatch, Observed};
use verified_server::limits::ServerLimits;
use verified_server::shell::Planted;
use verified_sim::directed::{self, Directed, DirectedError};
use verified_sim::scenario::{Fault, Scenario};
use verified_sim::{hosts, run};

/// Runs `seed` twice, compares the traces, then judges the world.
fn seed(seed: u64) {
    let outcome = run::twice(seed)
        .map_err(|error| {
            let tail: Vec<&String> = error.trace.iter().rev().take(40).rev().collect();
            format!("{error}\nlast trace lines:\n{tail:#?}")
        })
        .expect("the run finishes, deterministically");
    assert!(
        outcome.violations.is_empty(),
        "seed {seed} ({:?}) broke invariants:\n{:#?}",
        outcome.scenario,
        outcome.violations
    );
}

/// Different seeds must steer the world differently, or the seed is not
/// reaching it.
#[test]
fn seeds_steer_the_world() {
    let first = run::simulate(1).expect("seed 1 runs");
    let second = run::simulate(2).expect("seed 2 runs");
    assert_ne!(first.trace, second.trace, "two seeds made the same world");
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

/// The seeds the suite runs, each in its own test above.
const FIXED: [u64; 14] = [0, 1, 2, 3, 4, 5, 6, 7, 37, 50, 51, 53, 342, 818];

/// Fault paths the fixed seeds must reach, each as text its trace shows
/// (every needle on some line): a path nobody reaches is a path nobody
/// checks.
const PATHS: [(&str, &[&str]); 6] = [
    (
        "the TUI stops following (Down) and the user reconnects",
        &["User(\"reconnect\")"],
    ),
    (
        "the TUI's create gives up and the user retries it",
        &["User(\"retry creating\")"],
    ),
    (
        "a daemon-Anthropic partition fails a model call over the network, and the core retries it",
        &["the model link partitions", "Retry { fault: Network"],
    ),
    (
        "the daemon crashes after an edit is dispatched, and the edit resolves Unknown",
        &[
            ": edit notes.txt",
            "daemon crashes at Dispatched",
            "exit: Err(Unknown)",
        ],
    ),
    (
        "the daemon crashes while a command runs, and the command resolves Unknown",
        &[
            "Exec { command: \"sleep",
            "daemon crashes for",
            "exit: Err(Unknown)",
        ],
    ),
    (
        "two edits and a command proposed in one step all act",
        &[
            "Exec { command: \"touch m",
            "_1\"), effect",
            "_2\"), effect",
        ],
    ),
];

#[test]
fn the_fixed_seeds_reach_every_fault_path() {
    let mut reached: Vec<Vec<u64>> = vec![Vec::new(); PATHS.len()];
    for seed in FIXED {
        let outcome = run::simulate(seed).expect("the run finishes");
        for ((_, needles), seeds) in PATHS.iter().zip(&mut reached) {
            if needles
                .iter()
                .all(|needle| outcome.trace.iter().any(|line| line.contains(needle)))
            {
                seeds.push(seed);
            }
        }
    }
    for ((path, _), seeds) in PATHS.iter().zip(&reached) {
        assert!(!seeds.is_empty(), "no fixed seed reaches: {path}");
    }
}

/// Persist before acting (Law 2), as a mutation test: a daemon that starts
/// tool runs before their decision is durable leaves the model on some
/// seed, where the shell dispatches a tool run the model has not started,
/// because its decision is not durable yet.
#[test]
fn running_tools_before_their_dispatch_is_durable_fails_the_replay() {
    let planted = Planted {
        run_before_persist: true,
        ..Planted::default()
    };
    let caught = (0..16u64).any(|seed| {
        let outcome = run::simulate_with(seed, planted).expect("the run finishes");
        replay_says(&outcome, "no run the model started")
    });
    assert!(caught, "no seed's replay caught a tool run before persist");
}

/// Whether some violation of `outcome` is the replay's, saying `why`.
fn replay_says(outcome: &run::Outcome, why: &str) -> bool {
    outcome
        .violations
        .iter()
        .any(|violation| violation.invariant == "model" && violation.detail.contains(why))
}

/// At most once inside one dispatch, as a mutation test: a shell that runs
/// a tool twice and reports it once is caught by the replay at the second
/// act on the world, though the shell's own `ToolRan` is passed only once.
#[test]
fn running_a_tool_twice_in_one_dispatch_fails_the_replay() {
    let planted = Planted {
        run_twice: true,
        ..Planted::default()
    };
    let caught = (0..16u64).any(|seed| {
        let outcome = run::simulate_with(seed, planted).expect("the run finishes");
        replay_says(&outcome, "acts on the world a second time")
    });
    assert!(caught, "no seed's replay caught a second run");
}

/// The model's `Execute` is the run acting on the world, not its return:
/// seed 37's command starts (a mutation) and the daemon crashes before it
/// returns, so the crash comes after the `Unsafe` run acted.
#[test]
fn a_crash_while_a_command_runs_is_after_it_acted() {
    let outcome = run::simulate(37).expect("seed 37 runs");
    let replayed = outcome.replayed.expect("seed 37 replays");
    assert!(
        replayed.keys.contains(&Key::Crashed {
            class: Class::Unsafe,
            ran: true,
        }),
        "seed 37 crashes after its command acted: {:?}",
        replayed.keys
    );
}

/// What the shell makes durable is what the core decided, as a mutation
/// test: a shell that persists an `Unknown` result as `Interrupted` (U4,
/// broken below the core) leaves the model where a group commit becomes
/// durable holding other records than the model's.
#[test]
fn persisting_unknown_as_interrupted_fails_the_replay() {
    let planted = Planted {
        unknown_as_interrupted: true,
        ..Planted::default()
    };
    let outcome = run::simulate_with(37, planted).expect("seed 37 runs");
    assert!(
        replay_says(&outcome, "the model made"),
        "seed 37 (a command resolves Unknown) diverges at its commit: {:#?}",
        outcome.violations
    );
}

/// The model's properties are checked on every state a DST run passes
/// through: replayed through a model whose shell reruns `Unsafe` calls
/// after a crash, seed 37 (the daemon crashes while a command runs)
/// breaks at-most-once, though the same run replays cleanly through the
/// true model.
#[test]
fn a_property_breaks_on_a_dst_run_through_a_planted_model() {
    let outcome = run::simulate(37).expect("seed 37 runs");
    assert!(
        outcome.replayed.is_some(),
        "seed 37 replays: {:#?}",
        outcome.violations
    );
    let harness = Harness {
        bug: Some(Bug::RerunUnsafe),
        ..harness(hosts::limits().core, &outcome.observed)
    };
    let broken = match verified_model::replay(&harness, &outcome.observed) {
        Err(Mismatch::Violated { property, .. }) => Some(property),
        Ok(_) | Err(Mismatch::Diverged(_) | Mismatch::Unpinned { .. }) => None,
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
    for seed in FIXED {
        let outcome = run::simulate(seed).expect("the run finishes");
        assert!(
            outcome.violations.is_empty(),
            "seed {seed}: {:#?}",
            outcome.violations
        );
        let replayed = outcome.replayed.expect("a run without violations replays");
        assert!(
            replayed.max_frontier <= 1,
            "seed {seed}: the run pins the model's state"
        );
        taken.extend(replayed.actions.into_keys());
    }
    for kind in [
        "Send",
        "Answer",
        "Interrupt",
        "Execute",
        "Parts",
        "Finish",
        "Fail",
        "ToolDone",
        "Sync",
        "Crash",
        "Read",
        "Reconnect",
    ] {
        assert!(
            taken.contains(kind),
            "no fixed seed replays a {kind}: {taken:?}"
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
        let outcome = run::twice_scenario(&scenario)
            .map_err(|error| error.to_string())
            .expect("the run finishes, deterministically");
        assert!(
            outcome.violations.is_empty(),
            "seed {seed} (crash {crash:?}) on a small log broke invariants:\n{:#?}",
            outcome.violations
        );
        let filled = outcome
            .trace
            .iter()
            .position(|line| line.contains("Shell(Refused)"))
            .expect("the log fills");
        assert!(
            outcome
                .trace
                .iter()
                .skip(filled)
                .any(|line| line.contains("outcome: Cancelled")),
            "seed {seed}: the turn running as the log fills ends cancelled"
        );
        assert_eq!(
            outcome.observed.contains(&Observed::Refused),
            crash.is_some(),
            "seed {seed}: only the crash leaves a resume refused"
        );
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

/// Running together is the scenario's essential ordering, not decoration:
/// without it the same turn runs its two `Unsafe` calls one after the other.
#[test]
fn without_its_hold_a_directed_scenario_misses_its_target() {
    let directed = directed::named("two_unsafe_runs_act_at_once").expect("a directed scenario");
    assert!(
        directed.scenario.ran_together,
        "the scenario holds its runs"
    );
    let apart = Directed {
        scenario: Scenario {
            ran_together: false,
            ..directed.scenario
        },
        ..directed
    };
    assert!(
        matches!(directed::run(&apart), Err(DirectedError::Missed { .. })),
        "the target is reached without the hold, so the hold proves nothing"
    );
}

/// How many seeds the nightly sweep runs.
const SWEEP: u64 = 500;

#[test]
#[ignore = "nightly sweep; run with --ignored, or `just sweep`"]
fn sweep() {
    for index in 0..SWEEP {
        seed(1_000u64.saturating_add(index));
    }
}
