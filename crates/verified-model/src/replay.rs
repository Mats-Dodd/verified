//! Replaying an observed run through the model.
//!
//! A run (the simulation's ledger, folded) is a sequence of [`Observed`]
//! facts in the order they happened. Each one either is a step of the
//! model (an event entering the core, a group commit becoming durable, a
//! boot after a crash, a tool's run acting on the world, the client folding
//! a record) or is a check on the state the model is in (a tool dispatched
//! must be a run the model has started, and a run acts on the world at
//! most once). [`replay`] walks the model along them and checks every
//! `always` property on every state it reaches.
//!
//! The model's `Execute` is the run acting on the world, so it is taken
//! where the run first did: its first [`Observed::Mutated`]. A run that
//! never mutates (a `read`, an `edit` whose file changed under it) acted
//! when it returned ([`Observed::Ran`]), unless it reports `Interrupted`:
//! then it was stopped before it acted. A run that returned and was never
//! reported (the daemon crashed first) is taken to have acted.
//!
//! The model is followed as a frontier: every state consistent with the
//! observations so far. Each step's values come from the observation (ids,
//! sizes, what the model proposed, the records decided and made durable),
//! so the frontier stays small; it is bounded by [`FRONTIER`] all the same,
//! and a run whose frontier empties has done something the model does not
//! allow: a [`Mismatch`] says where, what was observed, the state the model
//! was in and what it allowed there.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use stateright::{Expectation, Model as _};
use verified_core::{Event, ModelExit};
use verified_domain::{Abstract, Cause, EffectId, Exit, Limits};

use crate::coverage::{self, Key};
use crate::harness::{Action, Harness, Message, Offers};
use crate::world::{Job, Rec, World};

/// The most states the frontier may hold: past it, the observations no
/// longer pin the model's state, and a replay could hide a bug behind a
/// state the run was never in.
pub const FRONTIER: usize = 64;

/// One fact a run observed, in α of its values.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Observed {
    /// The daemon booted on a log holding exactly these records (a new
    /// session: none). Every boot but the first follows a crash.
    Booted(Vec<Rec>),
    /// The shell resumed, and `resume` decided these records.
    Resumed(Vec<Rec>),
    /// An event entered `decide`, which decided these records.
    Decided(Event<Abstract>, Vec<Rec>),
    /// A group commit from `first` became durable: these records.
    Acked { first: u64, records: Vec<Rec> },
    /// A tool run is about to start: its `Dispatched` is durable.
    Dispatched(EffectId),
    /// A tool run acted on the world: at most once per run, before it
    /// returns.
    Mutated(EffectId),
    /// A tool run returned: its side effect, if any, has happened. It acted
    /// here only if it never mutated and does not report `Interrupted`.
    Ran(EffectId),
    /// The client folded the record at this offset. Only the offset: what
    /// it folded there is the simulation's I6 to check, not the replay's.
    Read(u64),
}

/// The model's action for `event`: the action carries the event's values.
#[must_use]
pub fn action(event: &Event<Abstract>) -> Action {
    match event {
        Event::Message {
            message,
            turn,
            text,
        } => Action::Send(Message {
            message: *message,
            turn: *turn,
            bytes: text.bytes,
        }),
        Event::Approve { call, verdict } => Action::Answer(call.clone(), *verdict),
        Event::Interrupt { turn } => Action::Interrupt(*turn),
        Event::ModelParts { id, parts } => Action::Parts(*id, parts.bytes),
        Event::ModelDone {
            id,
            result: ModelExit::Finished(calls),
        } => Action::Finish(*id, calls.clone()),
        Event::ModelDone {
            id,
            result: ModelExit::Failed(cause),
        } => Action::Fail(*id, *cause),
        Event::ToolDone { id, exit, output } => Action::ToolDone(*id, *exit, output.bytes),
        Event::Woke { id } => Action::Woke(*id),
    }
}

/// A harness to replay `trace`, run under `limits`: its actions carry
/// every value `trace`'s events do.
#[must_use]
pub fn harness(limits: Limits, trace: &[Observed]) -> Harness {
    let actions: Vec<Action> = trace
        .iter()
        .filter_map(|observed| match observed {
            Observed::Decided(event, _) => Some(action(event)),
            Observed::Booted(_)
            | Observed::Resumed(_)
            | Observed::Acked { .. }
            | Observed::Dispatched(_)
            | Observed::Mutated(_)
            | Observed::Ran(_)
            | Observed::Read(_) => None,
        })
        .collect();
    Harness::trace(limits, Offers::of(&actions))
}

/// What a replay walked through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replayed {
    /// The observations replayed.
    pub observations: usize,
    /// How often each kind of action was taken (on any frontier state).
    pub actions: BTreeMap<&'static str, usize>,
    /// The largest frontier.
    pub max_frontier: usize,
    /// The coverage keys of every step taken (on any frontier state).
    pub keys: BTreeSet<Key>,
    /// The states the run may have ended in.
    pub ends: Vec<World>,
}

/// Observation `index` is nothing the model allows from any state the run
/// may have been in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub index: usize,
    pub observed: Observed,
    /// Why, for the first state of the frontier.
    pub why: String,
    /// That state, summarised.
    pub state: String,
    /// What the model allowed there.
    pub enabled: Vec<Action>,
}

/// Why a run is not a path of the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mismatch {
    Diverged(Box<Divergence>),
    /// After observation `index` (`None`: at boot) the model is in a state
    /// that breaks `property`.
    Violated {
        index: Option<usize>,
        property: &'static str,
        state: String,
    },
    /// After observation `index` the frontier outgrew [`FRONTIER`].
    Unpinned {
        index: usize,
        states: usize,
    },
}

impl fmt::Display for Mismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diverged(divergence) => {
                let Divergence {
                    index,
                    observed,
                    why,
                    state,
                    enabled,
                } = divergence.as_ref();
                write!(
                    formatter,
                    "the run left the model at observation {index}: {observed:?}\n  \
                 why: {why}\n  state: {state}\n  enabled: {enabled:?}"
                )
            }
            Self::Violated {
                index,
                property,
                state,
            } => write!(
                formatter,
                "after observation {index:?} the model breaks \"{property}\"\n  state: {state}"
            ),
            Self::Unpinned { index, states } => write!(
                formatter,
                "after observation {index} the run is in any of {states} model states \
                 (at most {FRONTIER})"
            ),
        }
    }
}

/// A state, short enough to read in a report.
#[must_use]
pub fn summary(world: &World) -> String {
    let flights: Vec<String> = world
        .flights
        .iter()
        .map(|(id, flight)| {
            let job = match &flight.job {
                Job::Model { batches } => format!("model({batches} batches)"),
                Job::Tool { call, tool } => format!("{tool:?} {}", call.as_str()),
                Job::Sleep => "sleep".to_owned(),
            };
            format!(
                "{}.{} {job}{}{}",
                id.epoch.0,
                id.seq,
                if flight.executed { " ran" } else { "" },
                if flight.cancelled { " cancelled" } else { "" },
            )
        })
        .collect();
    format!(
        "log {} records (last {:?}), pending {:?}, staged {:?}, flights [{}], asks {:?}, \
         client at {} (cursor {:?}), crashes {}",
        world.log.len(),
        world.log.last(),
        world.pending,
        world.staged,
        flights.join(", "),
        world.asks,
        world.client.next,
        world.client.cursor,
        world.crashes,
    )
}

/// The name of an action's kind.
#[must_use]
pub const fn kind(action: &Action) -> &'static str {
    match action {
        Action::Send(_) => "Send",
        Action::Answer(..) => "Answer",
        Action::Interrupt(_) => "Interrupt",
        Action::Execute(_) => "Execute",
        Action::Parts(..) => "Parts",
        Action::Finish(..) => "Finish",
        Action::Fail(..) => "Fail",
        Action::ToolDone(..) => "ToolDone",
        Action::Woke(_) => "Woke",
        Action::OrphanExecute(_) => "OrphanExecute",
        Action::OrphanComplete(_) => "OrphanComplete",
        Action::Duplicate => "Duplicate",
        Action::Sync => "Sync",
        Action::Crash(_) => "Crash",
        Action::Read => "Read",
        Action::Reconnect => "Reconnect",
    }
}

/// The actions `harness` enables at `world`.
fn enabled(harness: &Harness, world: &World) -> Vec<Action> {
    let mut actions = Vec::new();
    harness.actions(world, &mut actions);
    actions
}

/// Takes `action` from `world` if the model enables it there.
fn take(harness: &Harness, world: &World, action: &Action) -> Result<World, String> {
    if !enabled(harness, world).contains(action) {
        return Err(format!("{action:?} is not enabled"));
    }
    harness
        .next_state(world, action.clone())
        .ok_or_else(|| format!("{action:?} has no next state"))
}

/// The tool run `id` started in `world` and has not yet returned.
fn running(world: &World, id: EffectId) -> Result<(), String> {
    match world.flights.get(&id) {
        Some(flight) if matches!(flight.job, Job::Tool { .. }) && !flight.executed => Ok(()),
        Some(flight) => Err(format!(
            "effect {id:?} is {flight:?}, not a tool yet to run"
        )),
        None => Err(format!(
            "effect {id:?} is no run the model started (staged: {:?})",
            world.staged
        )),
    }
}

/// What the whole trace says about its tool runs, beside the state.
/// Effect ids are never reused (each boot is a new epoch), so each set is
/// the run's own.
struct Runs {
    /// The runs already seen acting on the world.
    acted: BTreeSet<EffectId>,
    /// The runs the trace reports `Interrupted`.
    interrupted: BTreeSet<EffectId>,
}

impl Runs {
    fn of(trace: &[Observed]) -> Self {
        let interrupted = trace
            .iter()
            .filter_map(|observed| match observed {
                Observed::Decided(
                    Event::ToolDone {
                        id,
                        exit: Exit::Err(Cause::Interrupted),
                        ..
                    },
                    _,
                ) => Some(*id),
                Observed::Booted(_)
                | Observed::Resumed(_)
                | Observed::Decided(..)
                | Observed::Acked { .. }
                | Observed::Dispatched(_)
                | Observed::Mutated(_)
                | Observed::Ran(_)
                | Observed::Read(_) => None,
            })
            .collect();
        Self {
            acted: BTreeSet::new(),
            interrupted,
        }
    }
}

/// The states `observed` leads to from `world`, each with the actions taken.
fn step(
    harness: &Harness,
    world: &World,
    observed: &Observed,
    runs: &Runs,
) -> Result<Vec<(World, Vec<Action>)>, String> {
    let one = |action: Action| -> Result<Vec<(World, Vec<Action>)>, String> {
        let next = take(harness, world, &action)?;
        Ok(vec![(next, vec![action])])
    };
    match observed {
        Observed::Booted(records) => {
            let mut found = Vec::new();
            for action in enabled(harness, world) {
                if let Action::Crash(_) = action
                    && let Some(next) = harness.next_state(world, action.clone())
                    && next.log == *records
                {
                    found.push((next, vec![action]));
                }
            }
            if found.is_empty() {
                return Err(format!(
                    "no crash leaves the {} records booted on (log {}, pending {})",
                    records.len(),
                    world.log.len(),
                    world.pending.len()
                ));
            }
            Ok(found)
        }
        Observed::Resumed(records) => {
            if world.pending != *records {
                return Err(format!(
                    "the model resumed to {:?}, the shell to {records:?}",
                    world.pending
                ));
            }
            Ok(vec![(world.clone(), Vec::new())])
        }
        Observed::Decided(event, records) => {
            let found = one(action(event))?;
            for (next, _) in &found {
                if next.pending != *records {
                    return Err(format!(
                        "the model decided {:?}, the shell {records:?}",
                        next.pending
                    ));
                }
            }
            Ok(found)
        }
        Observed::Acked { first, records } => {
            let at = usize::try_from(*first).map_err(|error| error.to_string())?;
            if at != world.log.len() {
                return Err(format!(
                    "a group from {at} became durable on a log of {}",
                    world.log.len()
                ));
            }
            let found = one(Action::Sync)?;
            for (next, _) in &found {
                if next.log.get(at..) != Some(records.as_slice()) {
                    return Err(format!(
                        "the model made {:?} durable, the log {records:?}",
                        next.log.get(at..)
                    ));
                }
            }
            Ok(found)
        }
        Observed::Dispatched(id) => {
            running(world, *id)?;
            Ok(vec![(world.clone(), Vec::new())])
        }
        Observed::Mutated(id) => {
            if runs.acted.contains(id) {
                return Err(format!(
                    "effect {id:?} acts on the world a second time in one run"
                ));
            }
            running(world, *id)?;
            one(Action::Execute(*id))
        }
        Observed::Ran(id) => match world.flights.get(id) {
            Some(flight) if matches!(flight.job, Job::Tool { .. }) => {
                if flight.executed || runs.interrupted.contains(id) {
                    Ok(vec![(world.clone(), Vec::new())])
                } else {
                    one(Action::Execute(*id))
                }
            }
            Some(_) | None => Err(format!(
                "effect {id:?} returned, but it is no tool run the model started"
            )),
        },
        Observed::Read(seq) => {
            let at = usize::try_from(*seq).map_err(|error| error.to_string())?;
            // The client reconnects only when it must: its connection is
            // down, or the read starts elsewhere than the connection's
            // cursor.
            if world.client.cursor == Some(at) {
                return one(Action::Read);
            }
            let reconnected = take(harness, world, &Action::Reconnect)?;
            if reconnected.client.cursor != Some(at) {
                return Err(format!(
                    "the client reads {at} but resumes from {:?}",
                    reconnected.client.cursor
                ));
            }
            let next = take(harness, &reconnected, &Action::Read)?;
            Ok(vec![(next, vec![Action::Reconnect, Action::Read])])
        }
    }
}

/// The first `always` property `world` breaks.
fn broken(harness: &Harness, world: &World) -> Option<&'static str> {
    harness
        .properties()
        .into_iter()
        .find(|property| {
            property.expectation == Expectation::Always && !(property.condition)(harness, world)
        })
        .map(|property| property.name)
}

/// Replays `trace` through `harness`, checking every `always` property on
/// every state it reaches.
///
/// # Errors
/// The trace does not start with a boot on an empty log, an observation is
/// nothing the model allows, a state breaks a property, or the
/// observations stop pinning the model's state.
pub fn replay(harness: &Harness, trace: &[Observed]) -> Result<Replayed, Mismatch> {
    let mut actions: BTreeMap<&'static str, usize> = BTreeMap::new();
    // The world boots once before anything is observed: the first
    // observation is that boot, on an empty log.
    let mut frontier = harness.init_states();
    let Some((first, rest)) = trace.split_first() else {
        return Err(Mismatch::Diverged(Box::new(Divergence {
            index: 0,
            observed: Observed::Booted(Vec::new()),
            why: "the trace is empty".to_owned(),
            state: frontier.first().map_or_else(String::new, summary),
            enabled: Vec::new(),
        })));
    };
    if *first != Observed::Booted(Vec::new()) {
        return Err(Mismatch::Diverged(Box::new(Divergence {
            index: 0,
            observed: first.clone(),
            why: "a run starts by booting on an empty log".to_owned(),
            state: frontier.first().map_or_else(String::new, summary),
            enabled: Vec::new(),
        })));
    }
    for world in &frontier {
        if let Some(property) = broken(harness, world) {
            return Err(Mismatch::Violated {
                index: None,
                property,
                state: summary(world),
            });
        }
    }
    let mut max_frontier = frontier.len();
    let mut runs = Runs::of(trace);
    let mut keys = BTreeSet::new();
    for (index, observed) in (1..).zip(rest) {
        let mut next: Vec<World> = Vec::new();
        let mut why = None;
        for world in &frontier {
            match step(harness, world, observed, &runs) {
                Ok(found) => {
                    for (state, taken) in found {
                        let mut at = world.clone();
                        for action in &taken {
                            let count = actions.entry(kind(action)).or_default();
                            *count = count.saturating_add(1);
                            if let Some(after) = harness.next_state(&at, action.clone()) {
                                keys.extend(coverage::keys(harness, &at, action, &after));
                                at = after;
                            }
                        }
                        if !next.contains(&state) {
                            next.push(state);
                        }
                    }
                }
                Err(reason) => {
                    why.get_or_insert(reason);
                }
            }
        }
        if next.is_empty() {
            let world = frontier.first();
            return Err(Mismatch::Diverged(Box::new(Divergence {
                index,
                observed: observed.clone(),
                why: why.unwrap_or_default(),
                state: world.map_or_else(String::new, summary),
                enabled: world.map_or_else(Vec::new, |world| enabled(harness, world)),
            })));
        }
        if next.len() > FRONTIER {
            return Err(Mismatch::Unpinned {
                index,
                states: next.len(),
            });
        }
        for world in &next {
            if let Some(property) = broken(harness, world) {
                return Err(Mismatch::Violated {
                    index: Some(index),
                    property,
                    state: summary(world),
                });
            }
        }
        max_frontier = max_frontier.max(next.len());
        frontier = next;
        if let Observed::Mutated(id) = observed {
            runs.acted.insert(*id);
        }
    }
    Ok(Replayed {
        observations: trace.len(),
        actions,
        max_frontier,
        keys,
        ends: frontier,
    })
}
