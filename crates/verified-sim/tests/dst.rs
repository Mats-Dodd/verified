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

use verified_core::Event;
use verified_domain::{Exit, ModelFault, Record, Reset, Unstarted};
use verified_model::coverage::{Class, Key, Phase, Resolved};
use verified_model::{Bug, Harness, Kind, Mismatch, Observed};
use verified_server::limits::ServerLimits;
use verified_server::shell::Planted;
use verified_server::shim::Observation;
use verified_sim::directed::{self, Directed, DirectedError};
use verified_sim::ledger::{Entry, Op};
use verified_sim::run::Outcome;
use verified_sim::scenario::{
    Aim, AimedCrash, CancelAt, Fault, Scenario, Steer, SteerAt, TurnPlan,
};
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
    // the edit resolves Unknown. Later it spawns a child, and stops it.
    seed_53 = 53;
    // Two edits and a command are proposed in one step, and all act.
    seed_54 = 54;
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
            .is_some_and(|before| {
                before.iter().any(|(actor, observed)| {
                    *actor == divergence.actor && *observed == Observed::Mutated(id)
                })
            })
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
    let broken = match verified_model::replay_actors(&harness, &outcome.observed) {
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

/// v0's limits, shrunk so a seed's few turns fill the log: one line, no
/// more.
fn a_small_log() -> ServerLimits {
    let mut limits = hosts::limits();
    limits.wire.max_message_text_bytes = 1_000;
    limits.tool_output_bytes = 1_000;
    limits.log_line_bytes = 16usize << 10u32;
    limits.log_file_bytes = u64::try_from(limits.log_line_bytes).expect("small");
    limits
}

/// Seeds whose turns fill [`a_small_log`] while a turn runs, and the
/// simulated millisecond of a crash added as it fills. The millisecond
/// moves whenever the TUI's traffic does: it is where seed 28 first refuses
/// an append.
const FILLING: [(u64, Option<u64>); 4] = [(28, None), (53, None), (77, None), (28, Some(37_659))];

/// Storage is the world: a full log fails stop (L3). From the first refused
/// append until the daemon boots again, nothing is acknowledged and no tool
/// run starts, and the run is still a path of the model: the refused
/// decisions never reached the core.
#[test]
fn a_full_log_fails_stop_and_replays() {
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
            .map_err(|error| {
                let tail = error
                    .trace()
                    .iter()
                    .rev()
                    .take(30)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>();
                format!("{error}\n{}", tail.join("\n"))
            })
            .expect("the run finishes, deterministically");
        let mut halted = false;
        let mut filled = false;
        for stamped in &outcome.entries {
            match &stamped.entry {
                Entry::Shell(Observation::Refused { .. }) => {
                    halted = true;
                    filled = true;
                }
                Entry::Boot { .. } => halted = false,
                Entry::Acked { records, .. } => {
                    assert!(
                        !halted,
                        "seed {seed}: acknowledged {records:?} after a refusal"
                    );
                }
                Entry::Shell(Observation::Passed(barrier)) => {
                    assert!(!halted, "seed {seed}: {barrier:?} after a refusal");
                }
                Entry::Created { .. }
                | Entry::Listed { .. }
                | Entry::Recovered { .. }
                | Entry::Mutation { .. }
                | Entry::Wrote { .. }
                | Entry::Fault(_)
                | Entry::User(_)
                | Entry::GaveUp { .. }
                | Entry::Read { .. }
                | Entry::Shell(_)
                | Entry::Malformed { .. }
                | Entry::Caught { .. } => {}
            }
        }
        assert!(filled, "seed {seed}: the log fills");
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

/// One run of `scenario`, with the defect `planted` in the daemon.
fn played(scenario: &Scenario, planted: Planted) -> Outcome {
    run::once(scenario, planted)
        .map_err(|error| error.to_string())
        .expect("the run finishes")
}

/// Whether `outcome` broke `invariant`.
fn broke(outcome: &Outcome, invariant: &str) -> bool {
    outcome
        .violations
        .iter()
        .any(|violation| violation.invariant == invariant)
}

/// A root that spawns a child, which runs a command of its own (the rules
/// file grants it).
fn a_spawn() -> Scenario {
    Scenario::of(vec![TurnPlan::granted("spawn helper bash touch h0")])
}

/// Each actor scenario a planted-bug test runs breaks nothing without its
/// plant, deterministically: the plants, not the scenarios, break them.
#[test]
fn the_actor_scenarios_break_nothing() {
    for scenario in [
        a_spawn(),
        a_spawn_crashing_at_its_delivery(),
        a_steer_lost_to_esc(),
        a_stopped_parent(),
        a_silent_run(),
        a_steer_while_a_command_runs(),
    ] {
        run::twice(&scenario)
            .map_err(|error| error.to_string())
            .expect("the run finishes, deterministically")
            .verdict()
            .map_err(|broken| broken.to_string())
            .expect("the run breaks nothing");
    }
}

/// A1 between logs, as a mutation test: a shell that delivers a spawn
/// before its `Dispatched` is durable is caught by the replay, at the
/// child's log, which the model has no license to create yet.
#[test]
fn delivering_a_spawn_before_its_dispatch_is_durable_fails_the_replay() {
    let outcome = played(&a_spawn(), Planted::DeliverBeforePersist);
    let child = outcome
        .observed
        .iter()
        .map(|(actor, _)| *actor)
        .find(|actor| {
            outcome
                .observed
                .first()
                .is_some_and(|(root, _)| root != actor)
        });
    assert!(
        matches!(
            &outcome.replayed,
            Err(Mismatch::Diverged(divergence)) if Some(divergence.actor) == child
        ),
        "the replay diverges at the child: {:?}",
        outcome.replayed
    );
}

/// O2, as a mutation test: a recipient that acknowledges a delivery
/// before its `Delivered` is durable lets its sender record `Ok` for an
/// envelope a crash could still lose. The replay refuses the sender's
/// result while the append is pending.
#[test]
fn acknowledging_before_the_append_fails_the_replay() {
    let outcome = played(&a_spawn(), Planted::AckBeforeAppend);
    assert!(
        matches!(
            &outcome.replayed,
            Err(Mismatch::Diverged(divergence))
                if matches!(divergence.observed, Observed::Decided(Event::ToolDone { exit: Exit::Ok, .. }, _))
        ),
        "the replay refuses the early Ok: {:?}",
        outcome.replayed
    );
}

/// A spawn whose child holds its prompt when the daemon crashes, before
/// the parent heard: the `Keyed` spawn runs again on resume.
fn a_spawn_crashing_at_its_delivery() -> Scenario {
    Scenario {
        aimed: Some(AimedCrash {
            aim: Aim::Delivered,
            occurrence: 0,
            down: Duration::from_millis(100),
        }),
        ..Scenario::of(vec![TurnPlan::granted("spawn helper reply done")])
    }
}

/// O2, as a mutation test: a shell that mints a fresh envelope id when it
/// runs a `Keyed` call again after a crash delivers it twice. I9 finds the
/// second envelope licensed by no record, and the replay finds no delivery
/// in flight that carries it.
#[test]
fn a_fresh_key_after_a_crash_delivers_twice() {
    let scenario = a_spawn_crashing_at_its_delivery();
    let fair = played(&scenario, Planted::None);
    assert!(
        fair.entries.iter().any(|stamped| matches!(
            &stamped.entry,
            Entry::Fault(fault) if fault.starts_with("daemon crashes at Delivered")
        )),
        "the crash lands between the append and the parent's result"
    );
    let outcome = played(&scenario, Planted::FreshKeyOnResume);
    assert!(broke(&outcome, "I9"), "{:#?}", outcome.violations);
    assert!(
        matches!(&outcome.replayed, Err(Mismatch::Diverged(_))),
        "{:?}",
        outcome.replayed
    );
}

/// A message that steers a slow turn, which the user then cancels before
/// any step carries the message: it starts the next turn.
fn a_steer_lost_to_esc() -> Scenario {
    Scenario::of(vec![TurnPlan {
        text: "slow 0".to_owned(),
        verdicts: Vec::new(),
        cancel: Some(CancelAt::Streaming),
        steer: Some(Steer {
            at: SteerAt::Streaming,
            text: "steer 0".to_owned(),
        }),
    }])
}

/// L4, as a mutation test: a shell that counts a steered message consumed
/// when it lands, not when a model call carries it, loses it to Esc. I10
/// finds it unread at quiescence, and the replay finds the next turn
/// missing from the log.
#[test]
fn consumption_recorded_at_injection_loses_a_steer_to_esc() {
    let scenario = a_steer_lost_to_esc();
    let fair = played(&scenario, Planted::None);
    let turns = fair
        .entries
        .iter()
        .filter(|stamped| {
            matches!(&stamped.entry, Entry::Acked { records, .. }
                if records.iter().any(|record| matches!(record, Record::TurnStarted { .. })))
        })
        .count();
    assert_eq!(
        turns, 2,
        "the steer starts the turn after the cancelled one"
    );
    let outcome = played(&scenario, Planted::ConsumeAtInjection);
    assert!(broke(&outcome, "I10"), "{:#?}", outcome.violations);
    assert!(
        matches!(&outcome.replayed, Err(Mismatch::Diverged(_))),
        "{:?}",
        outcome.replayed
    );
}

/// A message that steers a turn while its command runs.
fn a_steer_while_a_command_runs() -> Scenario {
    Scenario::of(vec![TurnPlan {
        steer: Some(Steer {
            at: SteerAt::ToolRunning,
            text: "steer 0".to_owned(),
        }),
        ..TurnPlan::granted("bash sleep 3000")
    }])
}

/// L5, as a mutation test: a history that renders a steer where it was
/// delivered sends the model the message between the command's `tool_use`
/// and its result. The fake provider refuses such a conversation (I13).
#[test]
fn splicing_a_steer_into_its_step_is_caught() {
    let outcome = played(&a_steer_while_a_command_runs(), Planted::SpliceSteering);
    assert!(broke(&outcome, "I13"), "{:#?}", outcome.violations);
}

/// A root that spawned a child, stopped by the user.
fn a_stopped_parent() -> Scenario {
    Scenario {
        stop: true,
        ..a_spawn()
    }
}

/// The stop cascade, as a mutation test: a shell that never delivers a
/// stop to a child leaves it running though its parent stopped (I12).
#[test]
fn a_stop_that_skips_a_child_fails() {
    let outcome = played(&a_stopped_parent(), Planted::StopSkipsChildren);
    assert!(broke(&outcome, "I12"), "{:#?}", outcome.violations);
}

/// A command that never reports, whatever it is told.
fn a_silent_run() -> Scenario {
    Scenario::of(vec![TurnPlan::granted("bash hang")])
}

/// B2, as a mutation test: a shell that arms no deadline waits on a
/// silent run for good, so its report is lost. I15 finds the run still
/// open long past its deadline, and I5 (H1: a result for every call at
/// quiescence) a world that never goes quiet; with deadlines it resolves
/// `Unknown` by it.
#[test]
fn a_cancel_that_waits_for_a_silent_run_fails() {
    let outcome = played(&a_silent_run(), Planted::NoDeadline);
    assert!(broke(&outcome, "I15"), "{:#?}", outcome.violations);
    assert!(broke(&outcome, "I5"), "{:#?}", outcome.violations);
}

/// L3, as a mutation test: a log that acknowledges appends it never
/// synced loses them to a crash. The replay finds a boot on fewer records
/// than were acknowledged.
#[test]
fn acknowledging_an_append_without_its_fsync_fails_the_replay() {
    let caught = (0..16u64).any(|seed| {
        match run::once(&Scenario::from_seed(seed), Planted::SkipFsync) {
            Ok(outcome) => matches!(
                outcome.replayed,
                Err(Mismatch::Diverged(divergence)) if matches!(divergence.observed, Observed::Booted(_))
            ),
            Err(_) => false,
        }
    });
    assert!(caught, "no seed's replay caught a lost acknowledged append");
}

/// Twelve greetings, then a command to approve, under a log of
/// `file_bytes`: some size refuses the decision that dispatches it.
fn a_log_that_fills_at_a_dispatch(file_bytes: u64) -> Scenario {
    let mut turns: Vec<TurnPlan> = (0..12u32)
        .map(|index| TurnPlan::granted(&format!("hello {index}")))
        .collect();
    turns.push(TurnPlan::granted("bash touch f0"));
    let mut limits = hosts::limits();
    limits.wire.max_message_text_bytes = 1_000;
    limits.tool_output_bytes = 1_000;
    limits.log_line_bytes = 13_200;
    limits.log_file_bytes = file_bytes;
    Scenario {
        rules: None,
        limits,
        ..Scenario::of(turns)
    }
}

/// L3's fail-stop, as a mutation test: a shell that starts a refused
/// decision's effects acts without a durable record. Some log size in a
/// range (the sizes of ids and texts move it a little) refuses the
/// decision that dispatches the command; I16 finds the run started after
/// the refusal, though the fair shell breaks nothing there.
#[test]
fn acting_after_a_refused_append_fails_the_check() {
    let caught = (14_200..15_400).step_by(40).find(|file_bytes| {
        broke(
            &played(
                &a_log_that_fills_at_a_dispatch(*file_bytes),
                Planted::ActAfterRefusal,
            ),
            "I16",
        )
    });
    let file_bytes = caught.expect("some log size catches an effect after a refusal");
    run::twice(&a_log_that_fills_at_a_dispatch(file_bytes))
        .map_err(|error| error.to_string())
        .expect("the run finishes, deterministically")
        .verdict()
        .map_err(|broken| broken.to_string())
        .expect("the fair shell fails stop there and breaks nothing");
}

/// B3 in the simulation: two actors that answer each other's every
/// message chatter until the limits stop them, and the world goes quiet.
#[test]
fn chattering_actors_go_quiet_within_their_limits() {
    let mut scenario = Scenario::of(vec![TurnPlan::granted("spawn helper ping")]);
    let mut config = verified_domain::LimitsConfig::v0();
    config.turns_per_actor = core::num::NonZeroUsize::new(6).expect("non-zero");
    config.messages_per_actor = core::num::NonZeroUsize::new(6).expect("non-zero");
    scenario.limits.core = verified_domain::Limits::new(config);
    let outcome = run::twice(&scenario)
        .map_err(|error| error.to_string())
        .expect("the run finishes, deterministically");
    let pings = outcome
        .entries
        .iter()
        .filter(|stamped| {
            matches!(&stamped.entry, Entry::Acked { records, .. }
                if records.iter().any(|record| matches!(
                    record,
                    Record::Delivered { envelope } if matches!(envelope.from, verified_domain::Address::Actor(_))
                )))
        })
        .count();
    assert!(
        pings >= 6,
        "they chattered: {pings} deliveries between them"
    );
    outcome
        .verdict()
        .map_err(|broken| broken.to_string())
        .expect("the chatter breaks nothing and goes quiet");
}
