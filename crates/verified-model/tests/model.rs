//! BFS of the model (`verified_model::Harness`) with small bounds: every
//! safety property holds and each example is reached, and each planted bug
//! in the shell is caught. Then paths through the model replayed as traces.

use stateright::{Checker as _, Expectation, Model as _};
use verified_core::{Event, ModelExit, Verdict};
use verified_domain::{
    Abstract, Body, CallId, EffectId, Epoch, Exit, Failure, Limit, Limits, Measured, MessageId,
    Proposal, Record, Request, Shape, StepId, Tool, TurnId, TurnOutcome, Unit, UnixMillis,
    Unstarted,
};
use verified_model::coverage::{self, Class, Key, Phase, Resolved, Situation, explore};
use verified_model::replay::Divergence;
use verified_model::{
    Action, Bounded, Bug, Harness, Job, Kind, Mismatch, Observed, World, born, replay, suite,
};

/// Checks every `always` property, and that each `reachable` example
/// occurs (so the `always` checks are not vacuous). Returns the states
/// explored.
fn check(harness: Harness, reachable: &[&'static str]) -> usize {
    let properties = harness.properties();
    let checker = harness.checker().spawn_bfs().join();
    for property in properties {
        if property.expectation == Expectation::Always {
            let name = property.name;
            let counterexample = checker.discovery(name).map(stateright::Path::into_actions);
            assert_eq!(counterexample, None, "{name} is violated");
        }
    }
    for name in reachable {
        assert!(checker.discovery(name).is_some(), "never reached: {name}");
    }
    checker.unique_state_count()
}

#[test]
fn interrupts_and_duplicates() {
    let states = check(
        suite::without_crashes(),
        &[
            "a tool runs",
            "a turn completes",
            "a turn is cancelled",
            "a step is retried",
            "a stale completion is refused",
            "a turn is cancelled after its model finished",
            "a model call streams a second batch",
        ],
    );
    assert!(states > 10_000, "a real state space: {states}");
}

#[test]
fn crashes_with_every_tool_class() {
    let states = check(
        suite::crashes_with_every_tool_class(),
        &[
            "a tool runs",
            "a turn completes",
            "an outcome is unknown",
            "a stale completion is refused",
            "a model call streams a second batch",
            "a call not awaiting one is answered",
        ],
    );
    assert!(states > 10_000, "a real state space: {states}");
}

#[test]
fn crashes_across_steps() {
    let states = check(
        suite::crashes_across_steps(),
        &[
            "a tool runs",
            "a turn completes",
            "a stale completion is refused",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn crashes_while_cancelling() {
    let states = check(
        suite::crashes_while_cancelling(),
        &[
            "a turn is cancelled",
            "an outcome is unknown",
            "a model call streams a second batch",
            "a call not awaiting one is answered",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn rerunning_an_unsafe_call_after_a_crash_is_caught() {
    for tool in [Tool::Edit, Tool::Bash] {
        let checker = Harness {
            bug: Some(Bug::RerunUnsafe),
            ..suite::with_crashes(vec![vec![tool]], 1)
        }
        .checker()
        .spawn_bfs()
        .join();
        assert!(
            checker
                .discovery("an Unsafe call is dispatched at most once across crashes")
                .is_some(),
            "the checker finds the second {tool:?} run"
        );
    }
}

#[test]
fn resolving_an_unsafe_call_known_after_a_crash_is_caught() {
    for tool in [Tool::Edit, Tool::Bash] {
        let checker = Harness {
            bug: Some(Bug::UnsafeResolvedNotStarted),
            ..suite::with_crashes(vec![vec![tool]], 1)
        }
        .checker()
        .spawn_bfs()
        .join();
        assert!(
            checker
                .discovery("after a crash a dispatched Unsafe call resolves Unknown")
                .is_some(),
            "the checker sees the {tool:?} call resolved as if it never ran"
        );
        // The log refuses it too ("every durable prefix recovers"): a
        // synthesized `NotStarted` for a dispatched call claims more than v
        // knows (H2).
        assert_eq!(
            checker
                .discovery("an Unsafe call is dispatched at most once across crashes")
                .map(stateright::Path::into_actions),
            None,
            "the {tool:?} run is never repeated"
        );
    }
}

#[test]
fn leaving_an_unsafe_call_dispatched_after_a_crash_is_caught() {
    for tool in [Tool::Edit, Tool::Bash] {
        let checker = Harness {
            bug: Some(Bug::LeaveDispatched),
            ..suite::with_crashes(vec![vec![tool]], 1)
        }
        .checker()
        .spawn_bfs()
        .join();
        assert!(
            checker
                .discovery("quiescent implies the turn ended")
                .is_some(),
            "the checker sees the {tool:?} call never resolved"
        );
        assert_eq!(
            checker
                .discovery("every durable prefix recovers")
                .map(stateright::Path::into_actions),
            None,
            "the log the {tool:?} call is left in still recovers"
        );
    }
}

/// A2: a grant names a rule in force that fits its call. A shell that
/// approves what it should ask about, by the first rule in force (the
/// default `read: allow`), is caught by the audit, and the core refuses
/// the log too.
#[test]
fn approval_by_a_rule_that_does_not_match_is_caught() {
    let checker = Harness {
        bug: Some(Bug::RuleApprovesAll),
        ..suite::with_crashes(vec![vec![Tool::Edit]], 1)
    }
    .checker()
    .spawn_bfs()
    .join();
    for name in [
        "every grant is the user's or a rule in force that fits",
        "every durable prefix recovers",
    ] {
        assert!(
            checker.discovery(name).is_some(),
            "the edit granted by `read: allow` is caught: {name}"
        );
    }
}

#[test]
fn calls_past_the_bounds() {
    let states = check(
        suite::calls_past_the_bounds(),
        &["a tool runs", "a turn completes", "a call fails by a limit"],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn unsafe_calls_run_together() {
    let states = check(
        suite::unsafe_calls_run_together(),
        &[
            "two Unsafe calls run at once",
            "a Safe call reruns after a crash",
            "an outcome is unknown",
            "a stale completion is refused",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn a_client_follows_the_stream() {
    let states = check(
        suite::a_client_follows_the_stream(),
        &[
            "the client catches up",
            "a client resumed mid-stream catches up",
            "a turn completes",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn serving_records_before_they_are_durable_is_caught() {
    let checker = Harness {
        bug: Some(Bug::ServePending),
        ..Harness::bounded(
            suite::limits(1),
            Bounded {
                client: Some(0),
                ..suite::crashing(vec![Vec::new()])
            },
        )
    }
    .checker()
    .spawn_bfs()
    .join();
    assert!(
        checker
            .discovery("the client sees a prefix of the durable log")
            .is_some(),
        "a crash tears records the client already saw"
    );
}

#[test]
fn skipping_a_record_on_reconnect_is_caught() {
    let checker = Harness {
        bug: Some(Bug::SkipOnReconnect),
        ..Harness::bounded(
            suite::limits(1),
            Bounded {
                client: Some(1),
                ..suite::crashing(vec![Vec::new()])
            },
        )
    }
    .checker()
    .spawn_bfs()
    .join();
    assert!(
        checker
            .discovery("the client sees a prefix of the durable log")
            .is_some(),
        "the resumed reader misses a record"
    );
}

#[test]
#[ignore = "larger bounds: minutes of BFS; run with --run-ignored only"]
fn everything_at_once() {
    check(
        Harness::bounded(
            suite::limits(2),
            Bounded {
                max_interrupts: 1,
                max_duplicates: 1,
                client: Some(1),
                ..suite::crashing(vec![Vec::new(), vec![Tool::Read], vec![Tool::Bash]])
            },
        ),
        &[],
    );
}

#[test]
#[ignore = "larger bounds: hours of BFS; run with --run-ignored only"]
fn larger_bounds() {
    check(
        Harness::bounded(
            suite::limits(3),
            Bounded {
                messages: 2,
                scripts: vec![
                    Vec::new(),
                    vec![Tool::Read],
                    vec![Tool::Edit, Tool::Bash],
                    vec![Tool::Bash, Tool::Read],
                ],
                batches: 2,
                stale_answers: 1,
                max_crashes: 2,
                max_interrupts: 1,
                max_duplicates: 1,
                client: Some(1),
            },
        ),
        &[],
    );
}

/// Follows `path` from the initial state: each action must be one the
/// model enables where it is taken, and every safety property must hold
/// after it. Returns the state reached.
fn follow(harness: &Harness, path: &[Action]) -> World {
    let properties = harness.properties();
    let mut world = harness
        .init_states()
        .into_iter()
        .next()
        .expect("one initial state");
    for (index, action) in path.iter().enumerate() {
        let mut enabled = Vec::new();
        harness.actions(&world, &mut enabled);
        assert!(
            enabled.contains(action),
            "step {index}: {action:?} is not enabled; the model enables {enabled:?}"
        );
        world = harness
            .next_state(&world, action.clone())
            .expect("an enabled action steps");
        for property in &properties {
            if property.expectation == Expectation::Always {
                assert!(
                    (property.condition)(harness, &world),
                    "step {index}: {action:?} breaks {}",
                    property.name
                );
            }
        }
    }
    world
}

/// The one action `pick` chooses among those the model enables at `world`.
fn enabled(harness: &Harness, world: &World, pick: impl Fn(&Action) -> bool) -> Action {
    let mut actions = Vec::new();
    harness.actions(world, &mut actions);
    let mut chosen = actions.iter().filter(|action| pick(action));
    let action = chosen.next().cloned().expect("an enabled action to pick");
    assert_eq!(chosen.next(), None, "one action to pick among {actions:?}");
    action
}

/// The world goes on while the shell persists: a tool started by an
/// earlier decision runs while the next decision is being persisted, and a
/// model call streams more than one batch. The model allowed neither before.
#[test]
fn the_world_runs_while_a_decision_is_persisted() {
    let harness = Harness::bounded(
        suite::limits(1),
        Bounded {
            batches: 2,
            ..suite::crashing(vec![vec![Tool::Read, Tool::Bash]])
        },
    );
    let mut path = Vec::new();
    let mut world = follow(&harness, &path);
    let mut take = |world: &mut World, pick: &dyn Fn(&Action) -> bool| {
        let action = enabled(&harness, world, pick);
        path.push(action);
        *world = follow(&harness, &path);
    };
    take(&mut world, &|action| *action == Action::Sync);
    take(&mut world, &|action| action.kind() == Kind::Send);
    take(&mut world, &|action| *action == Action::Sync);
    for _ in 0..2u8 {
        take(&mut world, &|action| action.kind() == Kind::Parts);
        take(&mut world, &|action| *action == Action::Sync);
    }
    take(&mut world, &|action| action.kind() == Kind::Finish);
    take(&mut world, &|action| *action == Action::Sync);
    // The read runs on policy's approval; the bash awaits the user, whose
    // approval is then being persisted.
    take(&mut world, &|action| {
        matches!(
            action,
            Action::Deliver(Event::Approve {
                verdict: Verdict::Approve,
                ..
            })
        )
    });
    assert!(world.persisting(), "the approval is being persisted");
    take(&mut world, &|action| matches!(action, Action::Execute(_)));
    assert!(
        world.runs.values().any(|(tool, _)| *tool == Tool::Read),
        "the read ran while the approval was persisted: {path:?}"
    );
}

/// `path` up to its first step only the model's world takes: orphans and
/// late duplicates never happen in a simulated run, and trace mode never
/// offers them.
fn observable(mut path: Vec<Action>) -> Vec<Action> {
    if let Some(unseen) = path.iter().position(|action| {
        matches!(
            action.kind(),
            Kind::OrphanExecute | Kind::OrphanComplete | Kind::Duplicate
        )
    }) {
        path.truncate(unseen);
    }
    path
}

/// A path BFS found replays in trace mode, as far as a run could take it,
/// where each action is offered only because the path carries its values.
#[test]
fn a_path_found_by_bfs_replays_in_trace_mode() {
    let bounded = suite::with_crashes(
        vec![Vec::new(), vec![Tool::Bash], vec![Tool::Edit, Tool::Read]],
        1,
    );
    let checker = bounded.clone().checker().spawn_bfs().join();
    for name in ["an outcome is unknown", "a turn completes", "a tool runs"] {
        let path = observable(
            checker
                .discovery(name)
                .expect("BFS reaches it")
                .into_actions(),
        );
        let trace = Harness::trace(bounded.limits.clone(), events(&path));
        assert_eq!(
            follow(&trace, &path),
            follow(&bounded, &path),
            "both modes reach the same state on {name}"
        );
    }
}

/// Trace mode carries the ids and sizes a run had, under the limits it ran
/// with: the context limit (600 kB in v0) is reached only by real sizes.
#[test]
fn a_trace_carries_real_ids_and_sizes() {
    let at = UnixMillis::new(1_760_000_000_000).expect("a 48-bit time");
    let mut message = verified_model::message(born().actor, MessageId::from_parts(at, &[7; 10]));
    if let Event::Deliver { envelope } = &mut message {
        envelope.body = Body::Text(size(2_000));
    }
    // Boot is records 0 to 2 in epoch 1 (the epoch, the actor's birth, the
    // default rules); the message is delivered at 3 and starts turn 4,
    // whose first model call is licensed by its `StepStarted` (record 5);
    // two batches are records 6 and 7, and the read is dispatched by record
    // 10, after `StepFinished` and `Approved`.
    let turn = TurnId(4);
    let model = EffectId {
        epoch: Epoch(1),
        seq: 5,
    };
    let read = EffectId {
        epoch: Epoch(1),
        seq: 10,
    };
    let call = Proposal::<Abstract> {
        id: CallId::parse("toolu_01XFDUDYJgAACzvnptvVoYEL").expect("a protocol id"),
        request: Request::Run {
            args: Shape::Read {
                subject: Vec::new(),
            },
        },
    };
    let run = |first: u64, second: u64| {
        let parts = |bytes: u64| {
            Action::Deliver(Event::ModelParts {
                id: model,
                parts: size(bytes),
            })
        };
        let path = [
            Action::Sync,
            Action::Deliver(message.clone()),
            Action::Sync,
            parts(first),
            Action::Sync,
            parts(second),
            Action::Sync,
            Action::Deliver(Event::ModelDone {
                id: model,
                result: ModelExit::Finished(vec![call.clone()]),
            }),
            Action::Sync,
            Action::Execute(read),
            Action::Deliver(Event::ToolDone {
                id: read,
                exit: Exit::Ok,
                output: size(4_096),
            }),
            Action::Sync,
        ];
        follow(&Harness::trace(Limits::v0(), events(&path)), &path).log
    };
    let ended = |log: &[Record<Abstract>]| {
        log.iter().find_map(|record| {
            if let Record::TurnEnded { outcome, .. } = record {
                Some(*outcome)
            } else {
                None
            }
        })
    };
    assert_eq!(
        ended(&run(300_000, 299_000)),
        Some(TurnOutcome::Failed(Failure::Limit(Limit::Context))),
        "2 kB of text, 599 kB of parts and 4 kB of output pass 600 kB"
    );
    let small = run(1, 1);
    assert_eq!(ended(&small), None, "the turn goes on");
    assert!(
        small.contains(&Record::StepStarted {
            step: StepId { turn, index: 1 },
            carries: Vec::new(),
        }),
        "a second step starts: {small:?}"
    );
}

/// Trace mode offers only what the trace carries: with a model call in
/// flight and no completion of it in the trace, none is enabled, so a
/// replay cannot invent one.
#[test]
fn trace_mode_offers_only_the_traces_values() {
    let at = UnixMillis::new(1_760_000_000_000).expect("a 48-bit time");
    let message = verified_model::message(born().actor, MessageId::from_parts(at, &[7; 10]));
    let path = [Action::Sync, Action::Deliver(message), Action::Sync];
    let trace = Harness::trace(Limits::v0(), events(&path));
    let world = follow(&trace, &path);
    assert!(
        world
            .flights
            .values()
            .any(|flight| matches!(flight.job, Job::Model { .. })),
        "a model call is in flight"
    );
    let mut actions = Vec::new();
    trace.actions(&world, &mut actions);
    assert!(
        !actions.iter().any(|action| matches!(
            action,
            Action::Deliver(Event::ModelParts { .. } | Event::ModelDone { .. })
        )),
        "no completion the trace lacks: {actions:?}"
    );
}

/// A size of `bytes`.
const fn size(bytes: u64) -> Measured<Unit> {
    Measured { body: Unit, bytes }
}

/// The events `path` delivers.
fn events(path: &[Action]) -> Vec<Event<Abstract>> {
    path.iter().filter_map(Action::event).cloned().collect()
}

/// What a run along `path` would observe: each step as the shell's hook,
/// the log, the tools and the client report it.
fn observations(harness: &Harness, path: &[Action]) -> Vec<Observed> {
    let mut world = follow(harness, &[]);
    let mut trace = vec![
        Observed::Booted(Vec::new()),
        Observed::Resumed(world.pending.clone()),
    ];
    for (taken, action) in path.iter().enumerate() {
        let next = follow(harness, path.get(..=taken).expect("a prefix"));
        if let Some(event) = action.event() {
            trace.push(Observed::Decided(event.clone(), next.pending.clone()));
        }
        match action {
            Action::Sync => trace.push(Observed::Acked {
                first: u64::try_from(world.log.len()).expect("a short log"),
                records: next.log.get(world.log.len()..).expect("grown").to_vec(),
            }),
            Action::Crash(_) => {
                trace.push(Observed::Booted(next.log.clone()));
                trace.push(Observed::Resumed(next.pending.clone()));
            }
            Action::Execute(id) => trace.push(Observed::Ran {
                id: *id,
                stopped: false,
            }),
            Action::Read => trace.push(Observed::Read(
                u64::try_from(world.client.next).expect("a short log"),
            )),
            // Seen as the event entering the core above, or not at all.
            Action::Deliver(_)
            | Action::OrphanExecute(_)
            | Action::OrphanComplete(_)
            | Action::Duplicate
            | Action::Reconnect => {}
        }
        world = next;
    }
    trace
}

/// `bounded` in trace mode, its deliveries carrying the values `trace`'s
/// do.
fn trace_mode(bounded: &Harness, trace: &[Observed]) -> Harness {
    Harness::trace(
        bounded.limits.clone(),
        trace.iter().filter_map(Observed::event).cloned(),
    )
}

/// A path BFS found to an `Unknown` outcome (a crash after a dispatch), up
/// to its first step no simulated run observes, and the harness it ran in.
fn path_to_an_unknown_outcome() -> (Harness, Vec<Action>) {
    let bounded = suite::with_crashes(
        vec![Vec::new(), vec![Tool::Bash], vec![Tool::Edit, Tool::Read]],
        1,
    );
    let checker = bounded.clone().checker().spawn_bfs().join();
    let path = observable(
        checker
            .discovery("an outcome is unknown")
            .expect("BFS reaches it")
            .into_actions(),
    );
    (bounded, path)
}

/// A path BFS found, observed as a run would observe it, replays; moved so
/// that a tool runs before its dispatch is durable, it does not, and the
/// report says where.
#[test]
fn an_observed_path_replays_and_a_tool_run_before_persist_does_not() {
    let (bounded, path) = path_to_an_unknown_outcome();
    let trace = observations(&bounded, &path);
    let replayed = replay(&trace_mode(&bounded, &trace), &trace).expect("an observed path replays");
    assert_eq!(
        replayed.keys,
        path_keys(&bounded, &path),
        "the replay keys the steps the path took"
    );
    assert_eq!(
        replayed.end,
        follow(&bounded, &path),
        "the replay ends where the path does"
    );
    // The tool's run, moved to just before the group commit holding its
    // `Dispatched` (an effect's seq is that record's) becomes durable.
    let (ran, id) = trace
        .iter()
        .enumerate()
        .find_map(|(at, observed)| {
            if let Observed::Ran { id, .. } = observed {
                Some((at, *id))
            } else {
                None
            }
        })
        .expect("a tool runs");
    let acked = trace
        .iter()
        .position(|observed| {
            if let Observed::Acked { first, records } = observed {
                (*first..).zip(records).any(|(seq, record)| {
                    seq == id.seq && matches!(record, Record::Dispatched { .. })
                })
            } else {
                false
            }
        })
        .expect("its dispatch became durable");
    assert!(acked < ran, "dispatched before it ran");
    let mut early = trace.clone();
    let moved = early.remove(ran);
    early.insert(acked, moved.clone());
    let divergence = match replay(&trace_mode(&bounded, &early), &early) {
        Err(Mismatch::Diverged(divergence)) => Some(divergence),
        Ok(_) | Err(Mismatch::Violated { .. }) => None,
    }
    .expect("a run before persist diverges");
    assert_eq!(divergence.index, acked, "where the run left the model");
    assert_eq!(divergence.observed, moved, "what it did there");
    assert!(
        divergence.enabled.contains(&Action::Sync),
        "the model awaits the commit: {divergence:?}"
    );
}

/// The coverage keys of every step of `path`.
fn path_keys(harness: &Harness, path: &[Action]) -> std::collections::BTreeSet<Key> {
    let mut keys = std::collections::BTreeSet::new();
    let mut world = follow(harness, &[]);
    for action in path {
        let next = harness
            .next_state(&world, action.clone())
            .expect("a step of the path");
        keys.extend(coverage::keys(harness, &world, action, &next));
        world = next;
    }
    keys
}

/// Coverage's BFS (`coverage::explore`) reaches exactly the states
/// Stateright's checker does, so every key it finds is one of a state the
/// model checks, and it misses none.
#[test]
fn coverage_explores_every_state_the_checker_does() {
    let harness = suite::crashes_across_steps();
    let checked = harness
        .clone()
        .checker()
        .spawn_bfs()
        .join()
        .unique_state_count();
    let explored = explore(&harness);
    assert_eq!(explored.states, checked, "the same state space");
    for (key, witness) in &explored.witnesses {
        assert!(
            path_keys(&harness, witness).contains(key),
            "the witness to {key} reaches it"
        );
    }
}

/// The keys say what happened, not only that something did: on paths BFS
/// found, a crash before an `Unsafe` run acted leaves it `Unknown` past a
/// crash, still `Dispatched`; one after it acted leaves it `Ran`; and a
/// crash mid-persist is keyed by what it kept.
#[test]
fn coverage_keys_name_what_the_path_did() {
    let harness = suite::with_crashes(vec![vec![Tool::Bash]], 1);
    let explored = explore(&harness);
    for (key, unknown) in [
        (
            Key::Crashed {
                class: Class::Unsafe,
                ran: false,
            },
            Phase::Dispatched,
        ),
        (
            Key::Crashed {
                class: Class::Unsafe,
                ran: true,
            },
            Phase::Ran,
        ),
    ] {
        let witness = explored.witnesses.get(&key).expect("BFS reaches it");
        let mut path = witness.clone();
        // The crash's own resume resolves the call: the witness ends there.
        assert!(matches!(path.last(), Some(Action::Crash(_))), "{path:?}");
        let keys = path_keys(&harness, &path);
        assert!(
            keys.contains(&Key::Call {
                class: Class::Unsafe,
                phase: unknown,
                crossed: true,
                resolved: Resolved::Unknown,
            }),
            "{key} resolves Unknown at {unknown:?}: {keys:?}"
        );
        path.pop();
        assert!(
            !path_keys(&harness, &path).contains(&key),
            "the crash, not an earlier step, is keyed"
        );
    }
    let torn = explored
        .witnesses
        .get(&Key::Situation(Situation::CrashTornGroup))
        .expect("BFS tears a group");
    let before = follow(
        &harness,
        torn.get(..torn.len().saturating_sub(1)).expect("a prefix"),
    );
    let keep = if let Some(Action::Crash(keep)) = torn.last() {
        Some(*keep)
    } else {
        None
    }
    .expect("a torn group is keyed at its crash");
    assert!(
        0 < keep && keep < before.pending.len(),
        "kept {keep} of {}",
        before.pending.len()
    );
}

/// Where `trace` leaves the model, and why.
fn divergence(harness: &Harness, trace: &[Observed]) -> Box<Divergence> {
    match replay(harness, trace) {
        Err(Mismatch::Diverged(divergence)) => Some(divergence),
        Ok(_) | Err(Mismatch::Violated { .. }) => None,
    }
    .expect("the tampered trace diverges")
}

/// Each of the replay's checks on a run's own facts has been seen to fail:
/// an observed path, tampered one way at a time, leaves the model exactly
/// where it was tampered, for the reason that check gives.
#[test]
fn a_tampered_observed_path_diverges_where_it_was_tampered() {
    let (bounded, path) = path_to_an_unknown_outcome();
    let trace = observations(&bounded, &path);
    let replays = |tampered: &[Observed]| trace_mode(&bounded, tampered);
    let first = |pick: &dyn Fn(&Observed) -> bool| {
        trace.iter().position(pick).expect("the path observes it")
    };
    // Where the trace sees each run return, the run, and its tool.
    let runs: Vec<(usize, EffectId, Tool)> = trace
        .iter()
        .enumerate()
        .filter_map(|(at, observed)| {
            let Observed::Ran { id, .. } = observed else {
                return None;
            };
            let executed = path
                .iter()
                .position(|action| *action == Action::Execute(*id))?;
            let before = follow(&bounded, path.get(..executed)?);
            if let Job::Tool { tool, .. } = before.flights.get(id)?.job {
                Some((at, *id, tool))
            } else {
                None
            }
        })
        .collect();
    let run = |class: verified_domain::Class| {
        runs.iter()
            .copied()
            .find(|(_, _, tool)| tool.class() == class)
            .expect("the path runs a tool of each class")
    };
    let (ran, id, _) = run(verified_domain::Class::Safe);

    // A completion the model does not enable there: the tool reports
    // before its run returned. The model's next state alone would take it.
    let done = first(
        &|observed| matches!(observed, Observed::Decided(Event::ToolDone { id: done, .. }, _) if *done == id),
    );
    assert!(ran < done, "the tool reports after it ran");
    let mut early = trace.clone();
    let moved = early.remove(done);
    early.insert(ran, moved);
    let found = divergence(&replays(&early), &early);
    assert_eq!(found.index, ran, "where the report came early: {found:?}");
    assert!(found.why.contains("is not enabled"), "{found:?}");

    // A `Safe` run acting on the world, which it only reads.
    let mut mutated = trace.clone();
    mutated.insert(ran, Observed::Mutated(id));
    let found = divergence(&replays(&mutated), &mutated);
    assert_eq!(found.index, ran, "the act: {found:?}");
    assert!(found.why.contains("changes nothing"), "{found:?}");

    // An `Unsafe` run acting on the world twice before it returns.
    let (ran, id, _) = run(verified_domain::Class::Unsafe);
    let mut twice = trace.clone();
    twice.insert(ran, Observed::Mutated(id));
    twice.insert(ran, Observed::Mutated(id));
    let found = divergence(&replays(&twice), &twice);
    assert_eq!(
        found.index,
        ran.saturating_add(1),
        "the second act: {found:?}"
    );
    assert!(found.why.contains("a second time"), "{found:?}");

    // The shell decided other records than the core does.
    let decided =
        first(&|observed| matches!(observed, Observed::Decided(_, records) if !records.is_empty()));
    let mut other = trace.clone();
    if let Some(Observed::Decided(_, records)) = other.get_mut(decided) {
        records.pop();
    }
    let found = divergence(&replays(&other), &other);
    assert_eq!(found.index, decided, "{found:?}");
    assert!(found.why.contains("the model decided"), "{found:?}");

    // A group commit made other records durable than were decided.
    let acked = first(
        &|observed| matches!(observed, Observed::Acked { records, .. } if !records.is_empty()),
    );
    let mut other = trace.clone();
    if let Some(Observed::Acked { records, .. }) = other.get_mut(acked) {
        records.pop();
    }
    let found = divergence(&replays(&other), &other);
    assert_eq!(found.index, acked, "{found:?}");
    assert!(found.why.contains("the model made"), "{found:?}");

    // A boot on a log that lost a durable record.
    let booted =
        first(&|observed| matches!(observed, Observed::Booted(records) if !records.is_empty()));
    let mut lost = trace.clone();
    if let Some(Observed::Booted(records)) = lost.get_mut(booted) {
        records.remove(0);
    }
    let found = divergence(&replays(&lost), &lost);
    assert_eq!(found.index, booted, "{found:?}");
    assert!(found.why.contains("no crash leaves"), "{found:?}");
}

/// A run stopped before it acted did not act: its return, observed
/// `stopped`, takes no `Execute`, so the call resolves `NotStarted` while
/// still `Dispatched`. The same report from a run that acted claims more
/// than v knows (H2), and the replay refuses it.
#[test]
fn a_stopped_run_did_not_act() {
    let harness = Harness::bounded(
        suite::limits(1),
        Bounded {
            max_interrupts: 1,
            ..suite::bounded(vec![vec![Tool::Read]], 1)
        },
    );
    let key = Key::Call {
        class: Class::Safe,
        phase: Phase::Dispatched,
        crossed: false,
        resolved: Resolved::NotStarted(Unstarted::Cancelled),
    };
    let path = explore(&harness)
        .witnesses
        .remove(&key)
        .expect("BFS interrupts a dispatched read");
    let id = if let Some(Action::Deliver(Event::ToolDone { id, .. })) = path.last() {
        Some(*id)
    } else {
        None
    }
    .expect("the witness ends at the read's report");
    let mut trace = observations(&harness, &path);
    let done = trace.len().saturating_sub(1);
    trace.insert(done, Observed::Ran { id, stopped: true });
    let replayed = replay(&trace_mode(&harness, &trace), &trace).expect("the run replays");
    assert_eq!(replayed.keys, path_keys(&harness, &path), "no act is taken");
    assert!(replayed.keys.contains(&key), "the read never started");
    let mut acted = observations(&harness, &path);
    acted.insert(done, Observed::Ran { id, stopped: false });
    let found = divergence(&trace_mode(&harness, &acted), &acted);
    assert_eq!(
        found.index,
        done.saturating_add(1),
        "the report is refused: {found:?}"
    );
}
