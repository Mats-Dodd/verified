//! Replaying an observed run through the model.
//!
//! A run (the simulation's ledger, folded) is a sequence of [`Observed`]
//! facts in the order they happened, each of one actor's (its log, its
//! shell, its runs). Actors take slots in the order the trace first names
//! them: the first root at slot 0, a child at the slot its spawn named. Each one either is a step of the
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
//! when it returned ([`Observed::Ran`]), unless it was stopped: then it
//! was stopped before it acted, whether or not its report ever arrived.
//!
//! The model is followed one state at a time. Each observation names the
//! actions it means there, with their values (ids, sizes, what the model
//! proposed, how many pending records a crash kept), and what the state
//! after them must agree with (the records decided and made durable). A
//! run is one path, and an observation the model does not allow from the
//! state the run is in is a [`Mismatch`]: it says where, what was
//! observed, the state the model was in and what it allowed there.

use core::fmt;
use std::collections::BTreeSet;

use stateright::{Expectation, Model as _};
use verified_core::Event;
use verified_domain::{Abstract, ActorId, Address, Class, EffectId, Tool};

use crate::coverage::{self, Key};
use crate::harness::{Action, Harness};
use crate::world::{Flight, Job, Landing, Rec, Slot, World};

/// One fact a run observed, in α of its values.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Observed {
    /// The daemon booted the actor on a log holding exactly these records
    /// (a new actor: none; a child's log is created by its spawn). Every
    /// other boot follows a crash, which reboots every actor.
    Booted(Vec<Rec>),
    /// The shell resumed, and `resume` decided these records.
    Resumed(Vec<Rec>),
    /// An event entered `decide`, which decided these records. An
    /// envelope from another actor (or a stop from `v`), and a child's
    /// birth, are a delivery's steps landing.
    Decided(Event<Abstract>, Vec<Rec>),
    /// A full log refused the boot's resume: nothing of it was written, and
    /// none of its effects start. The shell fails stop (T1): it decides
    /// nothing more until it boots again. A refused `Decided` never reaches
    /// the model: the simulation's fold drops it, since the core never took
    /// its event.
    Refused,
    /// A group commit from `first` became durable: these records.
    Acked { first: u64, records: Vec<Rec> },
    /// A tool run is about to start: its `Dispatched` is durable.
    Dispatched(EffectId),
    /// A tool run acted on the world: an `Unsafe` run, at most once, before
    /// it returns.
    Mutated(EffectId),
    /// A tool run returned: its side effect, if any, has happened. It acted
    /// here only if it never mutated and was not `stopped` (it reports
    /// `NotStarted`: it saw its cancel before acting).
    Ran { id: EffectId, stopped: bool },
    /// A tool run will never return: it ignores its cancel and its kill,
    /// so only v's deadline resolves it (P1).
    Hung(EffectId),
    /// The client folded the record at this offset of the first root's
    /// log. Only the offset: what it folded there is the simulation's I6
    /// to check, not the replay's.
    Read(u64),
}

impl Observed {
    /// The event this observation delivers to the core, if it delivers one.
    #[must_use]
    pub const fn event(&self) -> Option<&Event<Abstract>> {
        match self {
            Self::Decided(event, _) => Some(event),
            Self::Booted(_)
            | Self::Resumed(_)
            | Self::Refused
            | Self::Acked { .. }
            | Self::Dispatched(_)
            | Self::Mutated(_)
            | Self::Ran { .. }
            | Self::Hung(_)
            | Self::Read(_) => None,
        }
    }
}

/// What a replay walked through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replayed {
    /// The observations replayed.
    pub observations: usize,
    /// The coverage keys of every step taken.
    pub keys: BTreeSet<Key>,
    /// The state the run ended in.
    pub end: World,
}

/// Observation `index` is nothing the model allows from the state the run
/// was in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub index: usize,
    /// The actor it was of.
    pub actor: ActorId,
    pub observed: Observed,
    pub why: String,
    /// The state the run was in, summarised.
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
}

impl fmt::Display for Mismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diverged(divergence) => {
                let Divergence {
                    index,
                    actor,
                    observed,
                    why,
                    state,
                    enabled,
                } = divergence.as_ref();
                write!(
                    formatter,
                    "the run left the model at observation {index} of {actor}: {observed:?}\n  \
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
        }
    }
}

/// A state, short enough to read in a report.
#[must_use]
pub fn summary(world: &World) -> String {
    let actors: Vec<String> = world
        .actors
        .iter()
        .enumerate()
        .filter(|(_, actor)| actor.created)
        .map(|(slot, actor)| {
            let flights: Vec<String> = actor
                .flights
                .iter()
                .map(|(id, flight)| {
                    let job = match &flight.job {
                        Job::Model { batches } => format!("model({batches} batches)"),
                        Job::Tool { call, tool } => format!("{tool:?} {}", call.as_str()),
                        Job::Sleep => "sleep".to_owned(),
                        Job::Deliver {
                            to, what, landing, ..
                        } => format!("{what:?} to {to:?} {landing:?}"),
                    };
                    format!(
                        "{}.{} {job}{}{}{}",
                        id.epoch.0,
                        id.seq,
                        if flight.executed { " ran" } else { "" },
                        if flight.cancelled { " cancelled" } else { "" },
                        if flight.hung { " hung" } else { "" },
                    )
                })
                .collect();
            format!(
                "actor {slot}: log {} records (last {:?}), pending {:?}, staged {:?}, \
                 flights [{}], asks {:?}",
                actor.log.len(),
                actor.log.last(),
                actor.pending,
                actor.staged,
                flights.join(", "),
                actor.asks,
            )
        })
        .collect();
    format!(
        "{}; client at {} (cursor {:?}), crashes {}",
        actors.join("; "),
        world.client.next,
        world.client.cursor,
        world.crashes,
    )
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

/// The tool of run `id` of the actor at `slot`, which `world` started and
/// has not yet returned.
fn running(world: &World, slot: Slot, id: EffectId) -> Result<Tool, String> {
    let actor = world
        .actor(slot)
        .ok_or_else(|| format!("no actor at slot {slot}"))?;
    match actor.flights.get(&id) {
        Some(Flight {
            job: Job::Tool { tool, .. },
            executed: false,
            ..
        }) => Ok(*tool),
        Some(Flight {
            job: Job::Tool { .. },
            executed: true,
            ..
        }) => Err(format!(
            "effect {id:?} acts on the world a second time in one run"
        )),
        Some(flight) => Err(format!(
            "effect {id:?} is {flight:?}, not a tool yet to run"
        )),
        None => Err(format!(
            "effect {id:?} is no run the model started (staged: {:?})",
            actor.staged
        )),
    }
}

/// The steps of the delivery in flight whose next step lands at slot `to`
/// at stage `at`, carrying `envelope` if it names one. A spawn of a child
/// whose log exists already passes its creation unseen (the registry
/// answers that it exists), so its birth takes that step first.
fn landing(
    world: &World,
    to: Slot,
    at: Landing,
    envelope: Option<verified_domain::EnvelopeId>,
) -> Result<Vec<Action>, String> {
    let created = world.actor(to).is_some_and(|actor| actor.created);
    world
        .actors
        .iter()
        .enumerate()
        .find_map(|(slot, actor)| {
            actor
                .flights
                .iter()
                .find_map(|(id, flight)| match &flight.job {
                    Job::Deliver {
                        to: Some(recipient),
                        envelope: carried,
                        landing,
                        ..
                    } if *recipient == to
                        && envelope.is_none_or(|envelope| envelope == *carried) =>
                    {
                        if *landing == at {
                            Some(vec![Action::Land(slot, *id)])
                        } else if *landing == Landing::Create && at == Landing::Born && created {
                            Some(vec![Action::Land(slot, *id), Action::Land(slot, *id)])
                        } else {
                            None
                        }
                    }
                    Job::Model { .. } | Job::Tool { .. } | Job::Sleep | Job::Deliver { .. } => None,
                })
        })
        .ok_or_else(|| format!("no delivery in flight lands at slot {to} ({at:?})"))
}

/// Where the replay is between a crash and every actor's boot after it.
#[derive(Default)]
struct Rebooting {
    /// The actors yet to boot after the last crash.
    slots: BTreeSet<Slot>,
}

/// The actions observation `observed` of the actor at `slot` means at
/// `world`, in order: none for a check on the state, or for a change the
/// shell makes outside the model's actions (a refusal, and the resume after
/// one). A crash reboots every actor: the first boot after it means the
/// crash, keeping of each actor's pending records what its own boot in
/// `rest` shows.
fn means(
    world: &World,
    slot: Slot,
    observed: &Observed,
    rest: &[(Slot, Observed)],
    rebooting: &mut Rebooting,
) -> Result<Vec<Action>, String> {
    let actor = world
        .actor(slot)
        .ok_or_else(|| format!("no actor at slot {slot}"))?;
    match observed {
        Observed::Booted(records) => {
            if !actor.created && records.is_empty() {
                // A spawn creates the child's log.
                return landing(world, slot, Landing::Create, None);
            }
            if rebooting.slots.remove(&slot) {
                // The crash was taken at the first boot after it.
                return Ok(Vec::new());
            }
            // A crash is named by how many pending records each actor
            // kept, so a boot means exactly one; `agrees` checks it left
            // these.
            let kept = |slot: Slot, records: &[Rec]| {
                let log = world.actor(slot).map_or(0, |actor| actor.log.len());
                records.len().checked_sub(log).ok_or_else(|| {
                    format!(
                        "no crash leaves the {} records booted on (log {log})",
                        records.len(),
                    )
                })
            };
            let mut keep = Vec::new();
            for (other, actor) in world.actors.iter().enumerate() {
                if other == slot {
                    keep.push(kept(slot, records)?);
                } else if actor.created {
                    let booted = rest.iter().find_map(|(at, observed)| {
                        if let Observed::Booted(records) = observed
                            && *at == other
                        {
                            Some(records)
                        } else {
                            None
                        }
                    });
                    keep.push(booted.map_or(Ok(0), |records| kept(other, records))?);
                    rebooting.slots.insert(other);
                } else {
                    keep.push(0);
                }
            }
            Ok(vec![Action::Crash(keep)])
        }
        Observed::Resumed(_) => Ok(Vec::new()),
        Observed::Refused => {
            if actor.persisting() {
                Ok(Vec::new())
            } else {
                Err("the log refused a resume, but none is being persisted".to_owned())
            }
        }
        Observed::Decided(Event::Born { .. }, _) => landing(world, slot, Landing::Born, None),
        Observed::Decided(Event::Deliver { envelope }, _) if envelope.from != Address::User => {
            landing(world, slot, Landing::Queued, Some(envelope.id))
        }
        Observed::Decided(event, _) => Ok(vec![Action::Deliver(slot, event.clone())]),
        Observed::Acked { first, .. } => {
            let at = usize::try_from(*first).map_err(|error| error.to_string())?;
            if at == actor.log.len() {
                Ok(vec![Action::Sync(slot)])
            } else {
                Err(format!(
                    "a group from {at} became durable on a log of {}",
                    actor.log.len()
                ))
            }
        }
        Observed::Dispatched(id) => {
            running(world, slot, *id)?;
            Ok(Vec::new())
        }
        Observed::Mutated(id) => {
            let tool = running(world, slot, *id)?;
            if tool.class() == Class::Safe {
                return Err(format!(
                    "effect {id:?} is a {tool:?} run, which changes nothing in the world"
                ));
            }
            Ok(vec![Action::Execute(slot, *id)])
        }
        Observed::Ran { id, stopped } => match actor.flights.get(id) {
            Some(flight) if matches!(flight.job, Job::Tool { .. }) => {
                if flight.executed || *stopped {
                    Ok(Vec::new())
                } else {
                    Ok(vec![Action::Execute(slot, *id)])
                }
            }
            Some(_) | None => Err(format!(
                "effect {id:?} returned, but it is no tool run the model started"
            )),
        },
        Observed::Hung(id) => match actor.flights.get(id) {
            Some(Flight {
                job: Job::Tool { .. },
                hung: false,
                ..
            }) => Ok(vec![Action::Hang(slot, *id)]),
            Some(_) | None => Err(format!(
                "effect {id:?} hangs, but it is no tool run the model has in flight"
            )),
        },
        Observed::Read(seq) => {
            let at = usize::try_from(*seq).map_err(|error| error.to_string())?;
            // The client reconnects only when it must: its connection is
            // down, or the read starts elsewhere than the connection's
            // cursor.
            if world.client.cursor == Some(at) {
                Ok(vec![Action::Read])
            } else {
                Ok(vec![Action::Reconnect, Action::Read])
            }
        }
    }
}

/// The state the run is in after observation `observed` of the actor at
/// `slot`, whose actions took the model from `before` to `after`: `after`
/// itself, if it agrees with what was observed, or for a refusal, `after`
/// changed as the shell changed it.
fn agrees(before: &World, after: World, slot: Slot, observed: &Observed) -> Result<World, String> {
    let (Some(was), Some(now)) = (before.actor(slot), after.actor(slot)) else {
        return Err(format!("no actor at slot {slot}"));
    };
    match observed {
        Observed::Booted(records) => {
            if now.log == *records {
                Ok(after)
            } else {
                Err(format!(
                    "no crash leaves the {} records booted on (log {}, pending {})",
                    records.len(),
                    was.log.len(),
                    was.pending.len()
                ))
            }
        }
        Observed::Resumed(records) => {
            if now.refused {
                return Err(
                    "the shell resumed again after its log refused, without a boot".to_owned(),
                );
            }
            if now.pending == *records {
                Ok(after)
            } else {
                Err(format!(
                    "the model resumed to {:?}, the shell to {records:?}",
                    now.pending
                ))
            }
        }
        Observed::Refused => {
            let mut refused = after;
            if let Some(actor) = refused.actors.get_mut(slot) {
                actor.pending.clear();
                actor.staged.clear();
                actor.refused = true;
            }
            Ok(refused)
        }
        Observed::Decided(_, records) => {
            if now.pending == *records {
                Ok(after)
            } else {
                Err(format!(
                    "the model decided {:?}, the shell {records:?}",
                    now.pending
                ))
            }
        }
        Observed::Acked { records, .. } => {
            let made = now.log.get(was.log.len()..);
            if made == Some(records.as_slice()) {
                Ok(after)
            } else {
                Err(format!(
                    "the model made {made:?} durable, the log {records:?}"
                ))
            }
        }
        Observed::Read(seq) => {
            let read = u64::try_from(after.client.next.saturating_sub(1))
                .map_err(|error| error.to_string())?;
            if read == *seq {
                Ok(after)
            } else {
                Err(format!(
                    "the client reads {seq} but resumes from {:?}",
                    before.client.next
                ))
            }
        }
        Observed::Dispatched(_)
        | Observed::Mutated(_)
        | Observed::Ran { .. }
        | Observed::Hung(_) => Ok(after),
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

/// Replays `trace`, the observations of the first root alone, through
/// `harness`, checking every `always` property on every state it reaches.
///
/// # Errors
/// The trace does not start with a boot on an empty log, an observation is
/// nothing the model allows, or a state breaks a property.
pub fn replay(harness: &Harness, trace: &[Observed]) -> Result<Replayed, Mismatch> {
    let actor = harness.born.actor;
    let tagged: Vec<(ActorId, Observed)> = trace
        .iter()
        .map(|observed| (actor, observed.clone()))
        .collect();
    replay_actors(harness, &tagged)
}

/// Replays `trace`, the observations of several actors, through `harness`.
///
/// The observations are in the order they happened, each with its actor,
/// at the slot `harness` gives it ([`Harness::slot_of`]); every `always`
/// property is checked on every state the replay reaches.
///
/// # Errors
/// The trace does not start with the first root's boot on an empty log, it
/// names an actor `harness` has no slot for, an observation is nothing the
/// model allows, or a state breaks a property.
pub fn replay_actors(
    harness: &Harness,
    trace: &[(ActorId, Observed)],
) -> Result<Replayed, Mismatch> {
    // The world boots once before anything is observed: the first
    // observation is the first root's boot, on an empty log.
    let mut world = harness.boot();
    let root = harness.born.actor;
    let diverged =
        |index: usize, actor: ActorId, observed: &Observed, why: String, world: &World| {
            Mismatch::Diverged(Box::new(Divergence {
                index,
                actor,
                observed: observed.clone(),
                why,
                state: summary(world),
                enabled: enabled(harness, world),
            }))
        };
    let Some(((first_actor, first), rest)) = trace.split_first() else {
        return Err(diverged(
            0,
            root,
            &Observed::Booted(Vec::new()),
            "the trace is empty".to_owned(),
            &world,
        ));
    };
    if *first_actor != root || *first != Observed::Booted(Vec::new()) {
        return Err(diverged(
            0,
            *first_actor,
            first,
            "a run starts by booting the first root on an empty log".to_owned(),
            &world,
        ));
    }
    if let Some(property) = broken(harness, &world) {
        return Err(Mismatch::Violated {
            index: None,
            property,
            state: summary(&world),
        });
    }
    let mut slotted: Vec<(Slot, Observed)> = Vec::with_capacity(rest.len());
    for (index, (actor, observed)) in (1..).zip(rest) {
        let slot = harness.slot_of(*actor).ok_or_else(|| {
            diverged(
                index,
                *actor,
                observed,
                "the model has no slot for this actor".to_owned(),
                &world,
            )
        })?;
        slotted.push((slot, observed.clone()));
    }
    let mut keys = BTreeSet::new();
    let mut rebooting = Rebooting::default();
    for (index, ((slot, observed), (actor, _))) in (1..).zip(slotted.iter().zip(rest)) {
        let mut after = world.clone();
        let later = slotted.get(index..).unwrap_or_default();
        let stepped = means(&world, *slot, observed, later, &mut rebooting).and_then(|taken| {
            for action in taken {
                let next = take(harness, &after, &action)?;
                keys.extend(coverage::keys(harness, &after, &action, &next));
                after = next;
            }
            agrees(&world, after, *slot, observed)
        });
        world = stepped.map_err(|why| diverged(index, *actor, observed, why, &world))?;
        if let Some(property) = broken(harness, &world) {
            return Err(Mismatch::Violated {
                index: Some(index),
                property,
                state: summary(&world),
            });
        }
    }
    Ok(Replayed {
        observations: trace.len(),
        keys,
        end: world,
    })
}
