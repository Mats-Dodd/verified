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
use verified_domain::{Abstract, Cause, EffectId, Exit, Rerun, Tool};

use crate::coverage::{self, Key};
use crate::harness::{Action, Harness};
use crate::world::{Flight, Job, Rec, World};

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
    /// A full log refused the boot's resume: nothing of it was written, and
    /// none of its effects start. The shell interrupts the running turn
    /// and resumes again ([`Observed::Resumed`], on a log not being
    /// persisted). A refused `Decided` never reaches the model: the
    /// simulation's fold drops it, since the core never took its event.
    Refused,
    /// A group commit from `first` became durable: these records.
    Acked { first: u64, records: Vec<Rec> },
    /// A tool run is about to start: its `Dispatched` is durable.
    Dispatched(EffectId),
    /// A tool run acted on the world: an `Unsafe` run, at most once, before
    /// it returns.
    Mutated(EffectId),
    /// A tool run returned: its side effect, if any, has happened. It acted
    /// here only if it never mutated and does not report `Interrupted`.
    Ran(EffectId),
    /// The client folded the record at this offset. Only the offset: what
    /// it folded there is the simulation's I6 to check, not the replay's.
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
            | Self::Ran(_)
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

/// The tool of run `id`, which `world` started and has not yet returned.
fn running(world: &World, id: EffectId) -> Result<Tool, String> {
    match world.flights.get(&id) {
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
            world.staged
        )),
    }
}

/// The runs the trace reports `Interrupted`. Effect ids are never reused
/// (each boot is a new epoch), so each is the run's own.
fn interrupted(trace: &[Observed]) -> BTreeSet<EffectId> {
    trace
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
            | Observed::Refused
            | Observed::Decided(..)
            | Observed::Acked { .. }
            | Observed::Dispatched(_)
            | Observed::Mutated(_)
            | Observed::Ran(_)
            | Observed::Read(_) => None,
        })
        .collect()
}

/// The actions `observed` means at `world`, in order: none for a check on
/// the state, or for a change the shell makes outside the model's actions
/// (a refusal, and the resume after one).
fn means(
    harness: &Harness,
    world: &World,
    observed: &Observed,
    interrupted: &BTreeSet<EffectId>,
) -> Result<Vec<Action>, String> {
    match observed {
        Observed::Booted(records) => {
            // The crash is named by what it kept, so a boot means one; a
            // model that gains a second way to it stays visible here.
            assert!(
                enabled(harness, world)
                    .into_iter()
                    .filter(|action| matches!(action, Action::Crash(_)))
                    .filter_map(|crash| harness.next_state(world, crash))
                    .filter(|next| next.log == *records)
                    .count()
                    <= 1,
                "at most one crash leaves the log a boot found"
            );
            let kept = records.len().checked_sub(world.log.len()).ok_or_else(|| {
                format!(
                    "no crash leaves the {} records booted on (log {})",
                    records.len(),
                    world.log.len()
                )
            })?;
            Ok(vec![Action::Crash(kept)])
        }
        Observed::Resumed(_) => Ok(Vec::new()),
        Observed::Refused => {
            if world.persisting() {
                Ok(Vec::new())
            } else {
                Err("the log refused a resume, but none is being persisted".to_owned())
            }
        }
        Observed::Decided(event, _) => Ok(vec![Action::Deliver(event.clone())]),
        Observed::Acked { first, .. } => {
            let at = usize::try_from(*first).map_err(|error| error.to_string())?;
            if at == world.log.len() {
                Ok(vec![Action::Sync])
            } else {
                Err(format!(
                    "a group from {at} became durable on a log of {}",
                    world.log.len()
                ))
            }
        }
        Observed::Dispatched(id) => {
            running(world, *id)?;
            Ok(Vec::new())
        }
        Observed::Mutated(id) => {
            let tool = running(world, *id)?;
            if tool.rerun() == Rerun::Safe {
                return Err(format!(
                    "effect {id:?} is a {tool:?} run, which changes nothing in the world"
                ));
            }
            Ok(vec![Action::Execute(*id)])
        }
        Observed::Ran(id) => match world.flights.get(id) {
            Some(flight) if matches!(flight.job, Job::Tool { .. }) => {
                if flight.executed || interrupted.contains(id) {
                    Ok(Vec::new())
                } else {
                    Ok(vec![Action::Execute(*id)])
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
                Ok(vec![Action::Read])
            } else {
                Ok(vec![Action::Reconnect, Action::Read])
            }
        }
    }
}

/// The state the run is in after `observed`, whose actions took the model
/// from `before` to `after`: `after` itself, if it agrees with what was
/// observed, or for a refusal and the resume after one, `after` changed as
/// the shell changed it.
fn agrees(
    harness: &Harness,
    before: &World,
    after: World,
    observed: &Observed,
) -> Result<World, String> {
    match observed {
        Observed::Booted(records) => {
            if after.log == *records {
                Ok(after)
            } else {
                Err(format!(
                    "no crash leaves the {} records booted on (log {}, pending {})",
                    records.len(),
                    before.log.len(),
                    before.pending.len()
                ))
            }
        }
        Observed::Resumed(records) => {
            let resumed = if after.refused {
                harness
                    .resumed(&after)
                    .ok_or_else(|| "the log the shell resumes on does not recover".to_owned())?
            } else {
                after
            };
            if resumed.pending == *records {
                Ok(resumed)
            } else {
                Err(format!(
                    "the model resumed to {:?}, the shell to {records:?}",
                    resumed.pending
                ))
            }
        }
        Observed::Refused => {
            let mut refused = after;
            refused.pending.clear();
            refused.staged.clear();
            refused.refused = true;
            Ok(refused)
        }
        Observed::Decided(_, records) => {
            if after.pending == *records {
                Ok(after)
            } else {
                Err(format!(
                    "the model decided {:?}, the shell {records:?}",
                    after.pending
                ))
            }
        }
        Observed::Acked { records, .. } => {
            let made = after.log.get(before.log.len()..);
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
        Observed::Dispatched(_) | Observed::Mutated(_) | Observed::Ran(_) => Ok(after),
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
/// nothing the model allows, or a state breaks a property.
pub fn replay(harness: &Harness, trace: &[Observed]) -> Result<Replayed, Mismatch> {
    // The world boots once before anything is observed: the first
    // observation is that boot, on an empty log.
    let mut world = harness.boot();
    let diverged = |index: usize, observed: &Observed, why: String, world: &World| {
        Mismatch::Diverged(Box::new(Divergence {
            index,
            observed: observed.clone(),
            why,
            state: summary(world),
            enabled: enabled(harness, world),
        }))
    };
    let Some((first, rest)) = trace.split_first() else {
        return Err(diverged(
            0,
            &Observed::Booted(Vec::new()),
            "the trace is empty".to_owned(),
            &world,
        ));
    };
    if *first != Observed::Booted(Vec::new()) {
        return Err(diverged(
            0,
            first,
            "a run starts by booting on an empty log".to_owned(),
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
    let interrupted = interrupted(trace);
    let mut keys = BTreeSet::new();
    for (index, observed) in (1..).zip(rest) {
        let mut after = world.clone();
        let stepped = means(harness, &world, observed, &interrupted).and_then(|taken| {
            for action in taken {
                let next = take(harness, &after, &action)?;
                keys.extend(coverage::keys(harness, &after, &action, &next));
                after = next;
            }
            agrees(harness, &world, after, observed)
        });
        world = stepped.map_err(|why| diverged(index, observed, why, &world))?;
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
