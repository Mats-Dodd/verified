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

use verified_server::shell::Planted;
use verified_sim::run;

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
    // A reconnect resumes mid-turn: only the read-by-read I6 catches a
    // follower that skips a record there.
    seed_50 = 50;
    // The daemon is down from the start: the TUI's create gives up and the
    // user retries it by hand.
    seed_51 = 51;
    // The daemon crashes between an edit's marker and its rename, and the
    // rerun finishes the rename (I7); then the TUI link stays down past the
    // follower's reconnect schedule, so it stops following and the user
    // reconnects by hand.
    seed_818 = 818;
}

/// The seeds the suite runs, each in its own test above.
const FIXED: [u64; 11] = [0, 1, 2, 3, 4, 5, 6, 7, 50, 51, 818];

/// Fault paths the fixed seeds must reach, each as text its trace shows
/// (every needle on some line): a path nobody reaches is a path nobody
/// checks.
const PATHS: [(&str, &[&str]); 4] = [
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
        "the daemon crashes between an edit's marker and its rename",
        &["daemon crashes at EditMarked"],
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
/// tool runs before their decision is durable is caught by I1 on some seed.
#[test]
fn running_tools_before_their_dispatch_is_durable_is_caught() {
    let planted = Planted {
        run_before_persist: true,
    };
    let caught = (0..40u64).find(|seed| {
        run::simulate_with(*seed, planted).is_ok_and(|outcome| {
            outcome
                .violations
                .iter()
                .any(|violation| violation.invariant == "I1")
        })
    });
    assert!(caught.is_some(), "no seed caught a tool run before persist");
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
