//! Coverage: what a step of the model did, as a small key.
//!
//! A key is not a state. It names what happened on one step (one edge of
//! the model): how a call was resolved and how far it got first, a crash
//! with a tool run in flight, a kind of action, how a turn ended, or one of a few [`Situation`]s. Keys come from
//! the edge alone (the state before, the action, the state after), so BFS
//! can collect them for every edge it explores ([`explore`]) and a replay for
//! every step a run takes ([`Replayed::keys`](crate::Replayed::keys)). The
//! difference is the gap table: what the model reaches and the simulation
//! did not, each gap [`classify`]d.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use stateright::Model as _;
use verified_core::Event;
use verified_domain::{
    Cause, Exit, Failure, Fault, Limit, Record, Request, Rerun, Reset, Tool, TurnOutcome, named,
};

use crate::harness::{Action, Harness, Kind};
use crate::world::{Job, World};

/// A call's class: what its approval and a crash do to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    Safe,
    Unsafe,
    /// An unknown tool or arguments that did not parse: it never runs.
    Invalid,
}

impl Class {
    const fn of(tool: Tool) -> Self {
        match tool.rerun() {
            Rerun::Safe => Self::Safe,
            Rerun::Unsafe => Self::Unsafe,
        }
    }
}

/// How far a call got before its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    Proposed,
    Approved,
    Dispatched,
    /// Its run acted on the world (in any epoch).
    Ran,
}

/// How a call was resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resolved {
    Ok,
    Failed,
    /// Failed by a bound from `Limits`, before it ran.
    Limited,
    Interrupted,
    Unknown,
    Denied,
}

impl Resolved {
    const fn of(exit: Exit) -> Self {
        match exit {
            Exit::Ok => Self::Ok,
            Exit::Err(Cause::Failed(Fault::TooManyCalls)) => Self::Limited,
            Exit::Err(Cause::Failed(
                Fault::Overloaded | Fault::Network | Fault::Other | Fault::TooLarge,
            )) => Self::Failed,
            Exit::Err(Cause::Interrupted) => Self::Interrupted,
            Exit::Err(Cause::Unknown) => Self::Unknown,
            Exit::Err(Cause::Denied) => Self::Denied,
        }
    }
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ended {
    Completed,
    Cancelled,
    /// A bound from `Limits` ended it: steps or context.
    Steps,
    Context,
    /// The model call failed for good.
    Model,
}

impl Ended {
    const fn of(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed => Self::Completed,
            TurnOutcome::Cancelled => Self::Cancelled,
            TurnOutcome::Failed(Failure::Limit(Limit::Steps)) => Self::Steps,
            TurnOutcome::Failed(Failure::Limit(Limit::Context)) => Self::Context,
            TurnOutcome::Failed(Failure::Model(_)) => Self::Model,
        }
    }
}

/// A situation a step was in, beyond its action's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Situation {
    /// A crash lost the whole decision being persisted.
    CrashLostGroup,
    /// A crash kept part of the decision being persisted (a torn group).
    CrashTornGroup,
    /// A crash came after the decision was written, before it was acked.
    CrashKeptGroup,
    /// The client reconnected while a turn was running.
    ReconnectMidTurn,
    /// The user stopped a turn while it awaited an approval.
    InterruptAtPrompt,
    /// The user answered a call that was not awaiting an answer.
    StaleAnswer,
    /// A tool run acted while the shell persisted a later decision.
    RanWhilePersisting,
    /// The client read while the shell persisted a decision.
    ReadWhilePersisting,
    /// A model call streamed a second batch of parts.
    SecondBatch,
    /// Two `Unsafe` runs had acted and neither had reported.
    UnsafeTogether,
    /// A step was reset to retry its model call.
    StepRetried,
    /// A step was reset because the daemon restarted under it.
    StepRestarted,
}

/// One thing a step of the model did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A call was resolved: its class, how far it got, whether a crash
    /// came between its last dispatch and its result, and how.
    Call {
        class: Class,
        phase: Phase,
        crossed: bool,
        resolved: Resolved,
    },
    /// The daemon crashed with a dispatched tool run in flight, which had
    /// (`ran`) or had not yet acted on the world: the crash came after or
    /// before its execute.
    Crashed {
        class: Class,
        ran: bool,
    },
    /// An action of this kind was taken.
    Action(Kind),
    /// A turn ended so.
    Turn(Ended),
    Situation(Situation),
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call {
                class,
                phase,
                crossed,
                resolved,
            } => write!(
                formatter,
                "call {class:?} {phase:?}{} -> {resolved:?}",
                if *crossed { " crash" } else { "" }
            ),
            Self::Crashed { class, ran } => write!(
                formatter,
                "crash {} the {class:?} run acted",
                if *ran { "after" } else { "before" }
            ),
            Self::Action(kind) => write!(formatter, "action {kind:?}"),
            Self::Turn(ended) => write!(formatter, "turn {ended:?}"),
            Self::Situation(situation) => write!(formatter, "{situation:?}"),
        }
    }
}

/// The keys of the decision `after` holds (its records from `after.log`
/// on), read against every record before it.
fn decided(after: &World, keys: &mut BTreeSet<Key>) {
    let start = after.log.len();
    // Calls approved, and the position of each call's last `Dispatched`,
    // by number.
    let mut approved: BTreeSet<usize> = BTreeSet::new();
    let mut dispatched: BTreeMap<usize, usize> = BTreeMap::new();
    let mut bumped: Option<usize> = None;
    for (at, (record, target)) in named(after.log.iter().chain(&after.pending)).enumerate() {
        let new = at >= start;
        match record {
            Record::Approved { .. } => {
                if let Some((number, _)) = target {
                    approved.insert(number);
                }
            }
            Record::Dispatched { .. } => {
                if let Some((number, _)) = target {
                    dispatched.insert(number, at);
                }
            }
            Record::ToolResult { exit, .. } => {
                if let (true, Some((number, proposal))) = (new, target) {
                    let (class, ran) = match proposal.request {
                        Request::Run { args } => (
                            Class::of(args),
                            after
                                .runs
                                .get(&proposal.id)
                                .is_some_and(|(_, count)| *count > 0),
                        ),
                        Request::Invalid { .. } => (Class::Invalid, false),
                    };
                    let dispatched = dispatched.get(&number).copied();
                    let phase = if ran {
                        Phase::Ran
                    } else if dispatched.is_some() {
                        Phase::Dispatched
                    } else if approved.contains(&number) {
                        Phase::Approved
                    } else {
                        Phase::Proposed
                    };
                    keys.insert(Key::Call {
                        class,
                        phase,
                        crossed: dispatched
                            .is_some_and(|dispatched| bumped.is_some_and(|bump| bump > dispatched)),
                        resolved: Resolved::of(*exit),
                    });
                }
            }
            Record::EpochBumped { .. } => bumped = Some(at),
            Record::TurnEnded { outcome, .. } => {
                if new {
                    keys.insert(Key::Turn(Ended::of(*outcome)));
                }
            }
            Record::StepReset { reason, .. } => {
                if new {
                    keys.insert(Key::Situation(match reason {
                        Reset::Retry { .. } => Situation::StepRetried,
                        Reset::Restart => Situation::StepRestarted,
                    }));
                }
            }
            Record::TurnStarted { .. }
            | Record::StepFinished { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::CancelRequested { .. } => {}
        }
    }
}

const fn class(job: &Job) -> Option<Class> {
    match job {
        Job::Tool { tool, .. } => Some(Class::of(*tool)),
        Job::Model { .. } | Job::Sleep => None,
    }
}

/// The keys of the step `before` --`action`--> `after`.
#[must_use]
pub fn keys(harness: &Harness, before: &World, action: &Action, after: &World) -> BTreeSet<Key> {
    let mut keys = BTreeSet::from([Key::Action(action.kind())]);
    let situation = |keys: &mut BTreeSet<Key>, situation| {
        keys.insert(Key::Situation(situation));
    };
    match action {
        Action::Crash(keep) => {
            for flight in before.flights.values() {
                if let Some(class) = class(&flight.job) {
                    keys.insert(Key::Crashed {
                        class,
                        ran: flight.executed,
                    });
                }
            }
            let pending = before.pending.len();
            if pending > 0 {
                situation(
                    &mut keys,
                    if *keep == 0 {
                        Situation::CrashLostGroup
                    } else if *keep < pending {
                        Situation::CrashTornGroup
                    } else {
                        Situation::CrashKeptGroup
                    },
                );
            }
            decided(after, &mut keys);
        }
        Action::Execute(_) => {
            if before.persisting() {
                situation(&mut keys, Situation::RanWhilePersisting);
            }
            let unsafe_ran = after
                .flights
                .values()
                .filter(|flight| flight.executed && class(&flight.job) == Some(Class::Unsafe))
                .count();
            if unsafe_ran >= 2 {
                situation(&mut keys, Situation::UnsafeTogether);
            }
        }
        Action::Read => {
            if before.persisting() {
                situation(&mut keys, Situation::ReadWhilePersisting);
            }
        }
        Action::Reconnect => {
            if harness
                .core(before)
                .is_some_and(|core| core.running().is_some())
            {
                situation(&mut keys, Situation::ReconnectMidTurn);
            }
        }
        Action::Deliver(event) => {
            match event {
                Event::Interrupt { .. } => {
                    if !before.asks.is_empty() {
                        situation(&mut keys, Situation::InterruptAtPrompt);
                    }
                }
                Event::Approve { call, .. } => {
                    if !before.asks.contains(call) {
                        situation(&mut keys, Situation::StaleAnswer);
                    }
                }
                Event::ModelParts { id, .. } => {
                    if before.flights.get(id).is_some_and(
                        |flight| matches!(flight.job, Job::Model { batches } if batches >= 1),
                    ) {
                        situation(&mut keys, Situation::SecondBatch);
                    }
                }
                Event::Message { .. }
                | Event::ModelDone { .. }
                | Event::ToolDone { .. }
                | Event::Woke { .. } => {}
            }
            decided(after, &mut keys);
        }
        Action::OrphanExecute(_) | Action::OrphanComplete(_) | Action::Duplicate | Action::Sync => {
        }
    }
    keys
}

/// What BFS of a harness reached: how many states, and each key with the
/// first path found to it (BFS order: none shorter).
#[derive(Clone, Debug)]
pub struct Explored {
    pub states: usize,
    pub witnesses: BTreeMap<Key, Vec<Action>>,
}

fn fingerprint(world: &World) -> u64 {
    let mut hasher = DefaultHasher::new();
    world.hash(&mut hasher);
    hasher.finish()
}

/// BFS over every edge of `harness`, collecting each edge's keys. Unlike
/// Stateright's checker it sees edges, not only states, so a key reached
/// only along an edge into a state seen before is still found.
#[must_use]
pub fn explore(harness: &Harness) -> Explored {
    // Each state reached: the state it was first reached from, by which
    // action. A path is read back through them.
    let mut reached: Vec<(Option<usize>, Option<Action>)> = Vec::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut queue: VecDeque<(World, usize)> = VecDeque::new();
    let mut witnesses: BTreeMap<Key, Vec<Action>> = BTreeMap::new();
    for world in harness.init_states() {
        if seen.insert(fingerprint(&world)) {
            queue.push_back((world, reached.len()));
            reached.push((None, None));
        }
    }
    let path = |reached: &[(Option<usize>, Option<Action>)], mut at: Option<usize>| {
        let mut path = Vec::new();
        while let Some((parent, action)) = at.and_then(|index| reached.get(index)) {
            path.extend(action.iter().cloned());
            at = *parent;
        }
        path.reverse();
        path
    };
    let mut actions = Vec::new();
    while let Some((world, index)) = queue.pop_front() {
        actions.clear();
        harness.actions(&world, &mut actions);
        for action in &actions {
            let Some(next) = harness.next_state(&world, action.clone()) else {
                continue;
            };
            for key in keys(harness, &world, action, &next) {
                witnesses.entry(key).or_insert_with(|| {
                    let mut witness = path(&reached, Some(index));
                    witness.push(action.clone());
                    witness
                });
            }
            if seen.insert(fingerprint(&next)) {
                queue.push_back((next, reached.len()));
                reached.push((Some(index), Some(action.clone())));
            }
        }
    }
    Explored {
        states: reached.len(),
        witnesses,
    }
}

/// Why the model reaches a key the simulation does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gap {
    /// The model allows what the real daemon cannot do: harmless, it only
    /// makes the model's checks stronger.
    OverApproximation,
    /// The real world can do it, and the simulation's fake is kinder than
    /// the seam it replaces: what the simulation checks there is
    /// optimistic.
    Fidelity,
    /// The real daemon can do it and the simulation could, but no seed run
    /// did: a directed scenario's work.
    Test,
}

/// The class of a gap at `key`, and why.
#[must_use]
pub const fn classify(key: &Key) -> (Gap, &'static str) {
    const TEST: (Gap, &str) = (
        Gap::Test,
        "the real daemon can do it and the simulation could; no seed did",
    );
    match key {
        Key::Action(Kind::Duplicate) => (
            Gap::OverApproximation,
            "a run's task reports its completion once; the model redelivers one",
        ),
        Key::Action(Kind::OrphanComplete) => (
            Gap::OverApproximation,
            "a crashed daemon's tasks and channels die with its process; no \
             reply of theirs reaches the next daemon",
        ),
        Key::Action(Kind::OrphanExecute) => (
            Gap::Fidelity,
            "a real tool's child process outlives the daemon and may act after the \
             crash; the simulation's tool tasks die with the turmoil host",
        ),
        Key::Turn(Ended::Steps) => (
            Gap::Fidelity,
            "the fake model ends a turn after one round of tools; a real model may \
             call tools until the step limit",
        ),
        Key::Action(
            Kind::Send
            | Kind::Answer
            | Kind::Interrupt
            | Kind::Execute
            | Kind::Parts
            | Kind::Finish
            | Kind::Fail
            | Kind::ToolDone
            | Kind::Woke
            | Kind::Sync
            | Kind::Crash
            | Kind::Read
            | Kind::Reconnect,
        )
        | Key::Call { .. }
        | Key::Crashed { .. }
        | Key::Turn(_)
        | Key::Situation(_) => TEST,
    }
}
