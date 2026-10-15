//! BFS of the model (`verified_model::Harness`) with small bounds: every
//! safety property holds and each example is reached, and each planted bug
//! in the shell is caught. Then paths through the model replayed as traces.

use stateright::{Checker as _, Expectation, Model as _};
use verified_core::{Event, ModelExit, Summary, Verdict};
use verified_domain::{
    Abstract, ActorId, Body, CallId, EffectId, Epoch, Exit, Failure, Limit, Limits, Measured,
    MessageId, ModelFault, Proposal, Record, Request, Shape, StepId, Tool, TurnId, TurnOutcome,
    Unit, UnixMillis, Unstarted,
};
use verified_model::coverage::{self, Class, Key, Phase, Resolved, Situation, explore};
use verified_model::replay::Divergence;
use verified_model::{
    Action, Bounded, Bug, Harness, Job, Kind, Mismatch, Observed, World, born, replay,
    replay_actors, suite,
};

/// Checks every `always` and `eventually` property, and that each
/// `reachable` example occurs (so the checks are not vacuous). Returns the
/// states explored.
fn check(harness: Harness, reachable: &[&'static str]) -> usize {
    let properties = harness.properties();
    let checker = harness.checker().spawn_bfs().join();
    for property in properties {
        if property.expectation != Expectation::Sometimes {
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
        // knows (T3).
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

/// C1: a grant names a rule in force that fits its call. A shell that
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

/// C1: a shell that grants in the user's name what the user never
/// approved. `spec_apply` accepts a `Grant::User` (it cannot see the
/// command), so only the audit against the user's commands catches it.
#[test]
fn approval_by_the_user_without_a_command_is_caught() {
    let checker = Harness {
        bug: Some(Bug::UserApprovesAll),
        ..suite::with_crashes(vec![vec![Tool::Edit]], 1)
    }
    .checker()
    .spawn_bfs()
    .join();
    assert!(
        checker
            .discovery("every user's grant answers the user's approval command")
            .is_some(),
        "the edit granted without your command is caught"
    );
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
                batches: 2,
                stale_answers: 1,
                max_crashes: 2,
                max_interrupts: 1,
                max_duplicates: 1,
                client: Some(1),
                ..suite::bounded(
                    vec![
                        Vec::new(),
                        vec![Tool::Read],
                        vec![Tool::Edit, Tool::Bash],
                        vec![Tool::Bash, Tool::Read],
                    ],
                    2,
                )
            },
        ),
        &[],
    );
}

/// BFS of `harness` with `bug` planted finds a counterexample to each of
/// `caught`.
fn caught(harness: Harness, bug: Bug, caught: &[&'static str]) {
    let checker = Harness {
        bug: Some(bug),
        ..harness
    }
    .checker()
    .spawn_bfs()
    .join();
    for name in caught {
        assert!(
            checker.discovery(name).is_some(),
            "{bug:?} is caught by \"{name}\""
        );
    }
}

/// C2 and C1: sends and spawns across a crash on either side of the
/// delivery. An acknowledged delivery is in its recipient's log once, no
/// call takes effect twice, and every envelope follows its durable license.
#[test]
fn deliveries_across_crashes() {
    let states = check(
        suite::deliveries_across_crashes(),
        &[
            "an envelope is acknowledged",
            "a child is spawned",
            "a delivery lands again after a crash",
            "a turn completes",
        ],
    );
    assert!(states > 500, "a real state space: {states}");
}

/// C1: an outbox that delivers before the `Dispatched` licensing it is
/// durable; a crash then loses the license, and the envelope stays.
#[test]
fn delivering_before_the_license_is_durable_is_caught() {
    caught(
        suite::deliveries_across_crashes(),
        Bug::DeliverBeforeDurable,
        &["every delivery follows its durable license"],
    );
}

/// C2: a rerun sent under a fresh key after a crash is a second envelope
/// its recipient cannot tell from a new one.
#[test]
fn minting_a_fresh_key_on_resume_is_caught() {
    caught(
        suite::deliveries_across_crashes(),
        Bug::FreshKeyOnResume,
        &["no call is delivered twice"],
    );
}

/// C2 and T1: the sender is told its envelope landed before the append is
/// durable; a crash loses it.
#[test]
fn acknowledging_before_the_append_is_durable_is_caught() {
    caught(
        suite::deliveries_across_crashes(),
        Bug::AckBeforeDurable,
        &["an acknowledged delivery is in its recipient's durable log once"],
    );
}

/// T3 and T4: tasks across a crash end once each, and every task has ended
/// at quiescence; their notifications coalesce.
#[test]
fn tasks_across_crashes() {
    let states = check(
        suite::tasks_across_crashes(),
        &[
            "a task ends",
            "a step carries two notifications",
            "a turn completes",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

/// T3: a resume that ends again a task that already ended.
#[test]
fn ending_a_task_twice_after_a_crash_is_caught() {
    caught(
        suite::tasks_across_crashes(),
        Bug::EndTaskTwice,
        &[
            "every call and task resolves at most once",
            "every durable prefix recovers",
        ],
    );
}

/// T3 and T4: a resume that drops a running task's `Unknown` end leaves it
/// running for good, and its actor is never notified.
#[test]
fn leaving_a_task_running_after_a_crash_is_caught() {
    caught(
        suite::tasks_across_crashes(),
        Bug::LeaveTaskRunning,
        &["at quiescence every task has ended"],
    );
}

/// T3: runs report while Esc, a `cancel` and their deadlines race them; a
/// known outcome is never followed by another.
#[test]
fn completions_race_cancels_and_deadlines() {
    let states = check(
        suite::completions_race_cancels_and_deadlines(),
        &[
            "a turn is cancelled",
            "a task ends",
            "a stale completion is refused",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

/// T4: the user's messages steer a turn awaiting approval, with Esc
/// and a crash anywhere. Every envelope is carried once, at a step
/// boundary.
#[test]
fn steering_cancels_and_crashes() {
    let states = check(
        suite::steering_cancels_and_crashes(),
        &[
            "a call is denied by a steer",
            "a turn is cancelled",
            "a turn completes",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

/// T4: a steered message lands at the next step of its own turn.
#[test]
fn steering_across_steps() {
    let states = check(
        suite::steering_across_steps(),
        &[
            "a call is denied by a steer",
            "a steered message lands in its turn",
            "a turn is cancelled",
            "a turn completes",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

/// T4: a step that never carries the mail at the head of the mailbox
/// leaves it unread at quiescence.
#[test]
fn skipping_the_mail_at_a_step_is_caught() {
    caught(
        suite::steering_cancels_and_crashes(),
        Bug::SkipCarry,
        &["at quiescence every envelope is carried"],
    );
}

/// T4: a step that carries again what a step carried before.
#[test]
fn carrying_an_envelope_twice_is_caught() {
    caught(
        suite::steering_cancels_and_crashes(),
        Bug::CarryTwice,
        &["every envelope is carried at most once"],
    );
}

/// T4: a message spliced into a step whose calls are still running.
#[test]
fn splicing_a_message_into_a_step_is_caught() {
    caught(
        suite::steering_cancels_and_crashes(),
        Bug::SpliceSteering,
        &["mail is carried only at a step boundary"],
    );
}

/// P1: two actors answer each other's every message; neither waits for
/// the other, and the world settles within its limits.
#[test]
fn an_actor_cycle_goes_quiet() {
    let states = check(
        suite::an_actor_cycle_goes_quiet(),
        &["an envelope is acknowledged", "a call fails by a limit"],
    );
    assert!(states > 100, "a real state space: {states}");
}

/// P1: a `send` that waits for its recipient's turn to end is an `ask`:
/// two actors asking each other never settle.
#[test]
fn a_blocking_send_deadlocks_and_is_caught() {
    caught(
        suite::an_actor_cycle_goes_quiet(),
        Bug::BlockingSend,
        &["the world settles"],
    );
}

/// P1: a run that goes silent resolves `Unknown` at its deadline.
#[test]
fn hung_runs_resolve_by_their_deadline() {
    let states = check(
        suite::hung_runs_resolve_by_their_deadline(),
        &["a run resolves at its deadline", "a task ends"],
    );
    assert!(states > 100, "a real state space: {states}");
}

/// P1: without a deadline, a silent run is waited on for ever.
#[test]
fn waiting_on_a_run_that_never_reports_is_caught() {
    caught(
        suite::hung_runs_resolve_by_their_deadline(),
        Bug::NoDeadline,
        &["the world settles"],
    );
}

/// P1: a chain of spawns stops at the cap on actors, and a stop of the
/// root reaches its child; the world settles within its limits.
#[test]
fn chatter_goes_quiet_within_limits() {
    let states = check(
        suite::chatter_goes_quiet_within_limits(),
        &[
            "a child is spawned",
            "a stop reaches a child",
            "a call fails by a limit",
        ],
    );
    assert!(states > 100, "a real state space: {states}");
}

/// P1: a registry that ignores its cap on actors.
#[test]
fn a_missing_actor_limit_is_caught() {
    caught(
        suite::chatter_goes_quiet_within_limits(),
        Bug::NoActorLimit,
        &["every actor stays within its limits"],
    );
}

/// C2 for a stop: the user stops a root whose child runs a command, with
/// deadlines and a crash anywhere. Every stop reaches its child.
#[test]
fn stops_across_deadlines_and_crashes() {
    let states = check(
        suite::stops_across_deadlines_and_crashes(),
        &["a child is spawned", "a stop reaches a child"],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

/// C2: a stop given up on at a deadline, then lost to a crash, never
/// reaches the child.
#[test]
fn giving_up_on_a_stop_at_its_deadline_is_caught() {
    caught(
        suite::stops_across_deadlines_and_crashes(),
        Bug::StopExpires,
        &["every stop reaches its child"],
    );
}

/// C2: a stop that lands while its child's spawn is in flight finds the
/// child unborn, and the spawn then births a child that runs on.
#[test]
fn a_stop_before_its_childs_birth_is_caught() {
    caught(
        suite::chatter_goes_quiet_within_limits(),
        Bug::StopBeforeBirth,
        &["every stop reaches its child"],
    );
}

#[test]
fn compactions_between_turns_and_at_the_first_step() {
    check(
        suite::compactions_between_turns_and_at_the_first_step(),
        &[
            "an idle compaction lands",
            "a compaction is superseded",
            "a forced compaction lets its turn go on",
            "a turn is cancelled",
        ],
    );
}

#[test]
fn a_compaction_after_tools() {
    check(
        suite::a_compaction_after_tools(),
        &[
            "a tool runs",
            "a forced compaction lets its turn go on",
            "a turn is cancelled",
        ],
    );
}

#[test]
fn compact_between_turns() {
    check(
        suite::compact_between_turns(),
        &[
            "an idle compaction lands",
            "a forced compaction lets its turn go on",
        ],
    );
}

#[test]
fn compactions_across_crashes() {
    check(
        suite::compactions_across_crashes(),
        &[
            "an idle compaction lands",
            "a compaction is superseded",
            "a forced compaction lets its turn go on",
        ],
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
    take(&mut world, &|action| *action == Action::Sync(0));
    take(&mut world, &|action| action.kind() == Kind::Send);
    take(&mut world, &|action| *action == Action::Sync(0));
    for _ in 0..2u8 {
        take(&mut world, &|action| action.kind() == Kind::Parts);
        take(&mut world, &|action| *action == Action::Sync(0));
    }
    take(&mut world, &|action| action.kind() == Kind::Finish);
    take(&mut world, &|action| *action == Action::Sync(0));
    // The read runs on policy's approval; the bash awaits the user, whose
    // approval is then being persisted.
    take(&mut world, &|action| {
        matches!(
            action,
            Action::Deliver(
                0,
                Event::Approve {
                    verdict: Verdict::Approve,
                    ..
                }
            )
        )
    });
    assert!(world.persisting(), "the approval is being persisted");
    take(&mut world, &|action| {
        matches!(action, Action::Execute(0, _))
    });
    assert!(
        world
            .root()
            .runs
            .values()
            .any(|(tool, _)| *tool == Tool::Read),
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
    // Past the bound, the beat compacts first (its cut is record 12, after
    // the read's result); a summarize call that fails parks the actor.
    let summarize = EffectId {
        epoch: Epoch(1),
        seq: 12,
    };
    let run = |first: u64, second: u64, tail: &[Action]| {
        let parts = |bytes: u64| {
            Action::Deliver(
                0,
                Event::ModelParts {
                    id: model,
                    parts: size(bytes),
                },
            )
        };
        let path = [
            Action::Sync(0),
            Action::Deliver(0, message.clone()),
            Action::Sync(0),
            parts(first),
            Action::Sync(0),
            parts(second),
            Action::Sync(0),
            Action::Deliver(
                0,
                Event::ModelDone {
                    id: model,
                    result: ModelExit::Finished(vec![call.clone()]),
                },
            ),
            Action::Sync(0),
            Action::Execute(0, read),
            Action::Deliver(
                0,
                Event::ToolDone {
                    id: read,
                    exit: Exit::Ok,
                    output: size(4_096),
                },
            ),
            Action::Sync(0),
        ];
        let path: Vec<Action> = path.iter().chain(tail).cloned().collect();
        follow(&Harness::trace(Limits::v0(), events(&path)), &path)
            .root()
            .log
            .clone()
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
    let failed = [
        Action::Deliver(
            0,
            Event::Summarized {
                id: summarize,
                summary: Summary::Failed(ModelFault::Other),
            },
        ),
        Action::Sync(0),
    ];
    let full = run(300_000, 299_000, &failed);
    assert!(
        full.contains(&Record::CompactionStarted { through: 12 }),
        "2 kB of text, 599 kB of parts and 4 kB of output pass 600 kB: {full:?}"
    );
    assert_eq!(
        ended(&full),
        Some(TurnOutcome::Failed(Failure::Limit(Limit::Context))),
        "no summary made room"
    );
    let small = run(1, 1, &[]);
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
    let path = [
        Action::Sync(0),
        Action::Deliver(0, message),
        Action::Sync(0),
    ];
    let trace = Harness::trace(Limits::v0(), events(&path));
    let world = follow(&trace, &path);
    assert!(
        world
            .root()
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
            Action::Deliver(_, Event::ModelParts { .. } | Event::ModelDone { .. })
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
        Observed::Resumed(world.root().pending.clone()),
    ];
    for (taken, action) in path.iter().enumerate() {
        let next = follow(harness, path.get(..=taken).expect("a prefix"));
        if let Some(event) = action.event() {
            trace.push(Observed::Decided(
                event.clone(),
                next.root().pending.clone(),
            ));
        }
        match action {
            Action::Sync(_) => trace.push(Observed::Acked {
                first: u64::try_from(world.root().log.len()).expect("a short log"),
                records: next
                    .root()
                    .log
                    .get(world.root().log.len()..)
                    .expect("grown")
                    .to_vec(),
            }),
            Action::Crash(_) => {
                trace.push(Observed::Booted(next.root().log.clone()));
                trace.push(Observed::Resumed(next.root().pending.clone()));
            }
            Action::Execute(_, id) => trace.push(Observed::Ran {
                id: *id,
                stopped: false,
            }),
            Action::Read => trace.push(Observed::Read(
                u64::try_from(world.client.next).expect("a short log"),
            )),
            // Seen as the event entering the core above, or not at all.
            Action::Deliver(..)
            | Action::OrphanExecute(..)
            | Action::OrphanComplete(..)
            | Action::Duplicate(_)
            | Action::Land(..)
            | Action::Hang(..)
            | Action::Reconnect => {}
        }
        world = next;
    }
    trace
}

/// What a run along `path` would observe of every actor: each step as
/// that actor's shell, log, tools and the client report it, a delivery's
/// steps as its recipient's.
fn observations_of(harness: &Harness, path: &[Action]) -> Vec<(ActorId, Observed)> {
    let id = |slot: usize| harness.id(slot).expect("an actor at the slot");
    let mut world = follow(harness, &[]);
    let mut trace = vec![
        (id(0), Observed::Booted(Vec::new())),
        (id(0), Observed::Resumed(world.root().pending.clone())),
    ];
    for (taken, action) in path.iter().enumerate() {
        let next = follow(harness, path.get(..=taken).expect("a prefix"));
        let pending = |slot: usize| next.actor(slot).expect("an actor").pending.clone();
        match action {
            Action::Deliver(slot, event) => {
                trace.push((id(*slot), Observed::Decided(event.clone(), pending(*slot))));
            }
            Action::Land(slot, effect) => {
                let (to, event) = harness
                    .landing(&world, *slot, *effect)
                    .expect("a delivery's next step");
                match event {
                    Some(event) => {
                        trace.push((id(to), Observed::Decided(event, pending(to))));
                    }
                    None if !world.actor(to).expect("an actor").created => {
                        trace.push((id(to), Observed::Booted(Vec::new())));
                        trace.push((id(to), Observed::Resumed(pending(to))));
                    }
                    // The registry answers that the child exists.
                    None => {}
                }
            }
            Action::Sync(slot) => {
                let before = world.actor(*slot).expect("an actor").log.len();
                trace.push((
                    id(*slot),
                    Observed::Acked {
                        first: u64::try_from(before).expect("a short log"),
                        records: next
                            .actor(*slot)
                            .expect("an actor")
                            .log
                            .get(before..)
                            .expect("grown")
                            .to_vec(),
                    },
                ));
            }
            Action::Crash(_) => {
                for (slot, actor) in next.actors.iter().enumerate() {
                    if actor.created {
                        trace.push((id(slot), Observed::Booted(actor.log.clone())));
                        trace.push((id(slot), Observed::Resumed(actor.pending.clone())));
                    }
                }
            }
            Action::Execute(slot, effect) => trace.push((
                id(*slot),
                Observed::Ran {
                    id: *effect,
                    stopped: false,
                },
            )),
            Action::Read => trace.push((
                id(0),
                Observed::Read(u64::try_from(world.client.next).expect("a short log")),
            )),
            Action::OrphanExecute(..)
            | Action::OrphanComplete(..)
            | Action::Duplicate(_)
            | Action::Hang(..)
            | Action::Reconnect => {}
        }
        world = next;
    }
    trace
}

/// `bounded` in trace mode for the actors of `trace`, its deliveries
/// carrying the values `trace`'s do.
fn traced(bounded: &Harness, trace: &[(ActorId, Observed)]) -> Harness {
    Harness::traced(
        bounded.limits.clone(),
        trace.iter().filter_map(|(actor, observed)| {
            Some((bounded.slot_of(*actor)?, observed.event()?.clone()))
        }),
    )
    .with_others(bounded.others.clone())
    .with_rules(bounded.rules.clone())
}

/// A path of two actors BFS found (a spawn and a send, a crash on either
/// side of a delivery), observed actor by actor as a run would observe it,
/// replays through the logs of both; with the child's delivery moved before
/// the parent's `Dispatched` is durable, it does not.
#[test]
fn a_path_of_two_actors_replays_and_a_delivery_before_its_license_does_not() {
    let bounded = suite::deliveries_across_crashes();
    let checker = bounded.clone().checker().spawn_bfs().join();
    for name in [
        "a delivery lands again after a crash",
        "an envelope is acknowledged",
        "a child is spawned",
    ] {
        let path = observable(
            checker
                .discovery(name)
                .expect("BFS reaches it")
                .into_actions(),
        );
        let trace = observations_of(&bounded, &path);
        let replayed = replay_actors(&traced(&bounded, &trace), &trace)
            .map_err(|mismatch| format!("{name}: {mismatch}"))
            .expect("the observed path replays");
        assert_eq!(
            replayed.end,
            follow(&bounded, &path),
            "the replay ends where the path does: {name}"
        );
    }
    let path = observable(
        checker
            .discovery("an envelope is acknowledged")
            .expect("BFS reaches it")
            .into_actions(),
    );
    let trace = observations_of(&bounded, &path);
    let child = bounded.id(1).expect("a child");
    // The child's log is created, then the parent's `Dispatched` of the
    // spawn: move the creation before it.
    let created = trace
        .iter()
        .position(|(actor, observed)| *actor == child && *observed == Observed::Booted(Vec::new()))
        .expect("the spawn creates the child");
    let licensed = trace
        .iter()
        .rposition(|(actor, observed)| {
            *actor != child
                && matches!(observed, Observed::Acked { records, .. } if records.iter().any(|record| matches!(record, Record::Dispatched { .. })))
        })
        .filter(|at| *at < created)
        .expect("the spawn is dispatched first");
    let mut early = trace.clone();
    let moved = early.remove(created);
    early.insert(licensed, moved);
    let found = match replay_actors(&traced(&bounded, &early), &early) {
        Err(Mismatch::Diverged(divergence)) => Some(divergence),
        Ok(_) | Err(Mismatch::Violated { .. }) => None,
    }
    .expect("a delivery before its license diverges");
    assert_eq!(found.index, licensed, "where it was moved to: {found:?}");
    assert_eq!(found.actor, child, "the child's log: {found:?}");
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
        divergence.enabled.contains(&Action::Sync(0)),
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
        keep.first().copied()
    } else {
        None
    }
    .expect("a torn group is keyed at its crash");
    assert!(
        0 < keep && keep < before.root().pending.len(),
        "kept {keep} of {}",
        before.root().pending.len()
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
                .position(|action| *action == Action::Execute(0, *id))?;
            let before = follow(&bounded, path.get(..executed)?);
            if let Job::Tool { tool, .. } = before.root().flights.get(id)?.job {
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
/// than v knows (T3), and the replay refuses it.
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
    let id = if let Some(Action::Deliver(_, Event::ToolDone { id, .. })) = path.last() {
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
