//! The run as a path of the Stateright model (`verified_model`).
//!
//! The ledger is the run's total order: turmoil runs every host on one
//! thread, and each entry is recorded as it happens, the shell's own
//! observations ([`Entry::Shell`]) on the daemon's task at the point they
//! name. [`observed`] folds it, in α, into the facts the model replays,
//! each of one actor: what each boot recovered of its log (a spawn's
//! creation of a child's log among them), each input entering its core and
//! what it decided, each of its group commits made durable, each of its
//! tool runs dispatched, acting and returning, and the offset of each
//! record of the root's log the TUI folded (the offset only: what it
//! folded there is I6's to check). A delivery between actors is its
//! recipient's decision. A decision a full log refused is no fact: the
//! core never took its event, so the fold drops it (a refused resume
//! stays, as [`Observed::Refused`]). [`replay`] walks the model along them,
//! over every actor's log, and checks its properties on every state.

use verified_domain::{ActorId, Limits, Record};
use verified_model::alpha::record;
use verified_model::{Harness, Mismatch, Observed, Replayed, alpha};
use verified_server::payload::Parsed;
use verified_server::shim::{Barrier, Observation};

use crate::ledger::{Entry, Op, Stamped, root};

fn records(records: &[Record<Parsed>]) -> Vec<Record<verified_domain::Abstract>> {
    records.iter().map(record).collect()
}

/// The facts of a run's ledger the model replays, in order, each with its
/// actor.
#[must_use]
pub fn observed(entries: &[Stamped]) -> Vec<(ActorId, Observed)> {
    let mut trace: Vec<(ActorId, Observed)> = Vec::new();
    let root = root(entries);
    for stamped in entries {
        match &stamped.entry {
            Entry::Created { actor } => trace.push((*actor, Observed::Booted(Vec::new()))),
            Entry::Recovered {
                actor,
                records: found,
            } => trace.push((*actor, Observed::Booted(records(found)))),
            Entry::Acked {
                actor,
                first,
                records: synced,
            } => trace.push((
                *actor,
                Observed::Acked {
                    first: *first,
                    records: records(synced),
                },
            )),
            Entry::Mutation {
                actor, effect, op, ..
            } => {
                trace.push((*actor, Observed::Mutated(*effect)));
                // The fake shell's `hang` never returns, whatever it is told.
                if matches!(op, Op::Exec { command } if command == "hang") {
                    trace.push((*actor, Observed::Hung(*effect)));
                }
            }
            Entry::Read { from, next, .. } => {
                if let Some(root) = root {
                    trace.extend((*from..*next).map(|seq| (root, Observed::Read(seq))));
                }
            }
            Entry::Shell(Observation::Resumed {
                actor,
                records: decided,
            }) => trace.push((*actor, Observed::Resumed(records(decided)))),
            Entry::Shell(Observation::Decided {
                actor,
                event,
                records: decided,
            }) => trace.push((
                *actor,
                Observed::Decided(alpha::event(event), records(decided)),
            )),
            Entry::Shell(Observation::Refused { actor }) => {
                // What was refused is the last decision the actor reported.
                let last = trace.iter().rposition(|(of, observed)| {
                    of == actor && matches!(observed, Observed::Decided(..) | Observed::Resumed(_))
                });
                match last {
                    Some(at) if matches!(trace.get(at), Some((_, Observed::Decided(..)))) => {
                        trace.remove(at);
                    }
                    Some(_) | None => trace.push((*actor, Observed::Refused)),
                }
            }
            Entry::Shell(Observation::Passed(Barrier::Dispatched { actor, id })) => {
                trace.push((*actor, Observed::Dispatched(*id)));
            }
            Entry::Shell(Observation::Passed(Barrier::ToolRan { actor, id, stopped })) => {
                trace.push((
                    *actor,
                    Observed::Ran {
                        id: *id,
                        stopped: *stopped,
                    },
                ));
            }
            Entry::Shell(
                Observation::Requested { .. }
                | Observation::Passed(
                    Barrier::GroupWritten { .. }
                    | Barrier::Delivering { .. }
                    | Barrier::Delivered { .. },
                ),
            )
            | Entry::Listed { .. }
            | Entry::Boot { .. }
            | Entry::Wrote { .. }
            | Entry::Fault(_)
            | Entry::User(_)
            | Entry::GaveUp { .. }
            | Entry::Malformed { .. }
            | Entry::Caught { .. } => {}
        }
    }
    trace
}

/// Replays `trace` through the model, under the daemon's core `limits`.
///
/// # Errors
/// The run is not a path of the model, or breaks one of its properties.
pub fn replay(limits: Limits, trace: &[(ActorId, Observed)]) -> Result<Replayed, Mismatch> {
    verified_model::replay_actors(&harness(limits, trace), trace)
}

/// The model in trace mode under the daemon's core `limits`, its
/// deliveries carrying the values `trace`'s do.
///
/// The first root is the one the daemon's first resume recorded, at slot
/// 0, and every other actor is at the slot after it, in the order the
/// trace first names them. The rules its shells load are the first a
/// resume recorded.
#[must_use]
pub fn harness(limits: Limits, trace: &[(ActorId, Observed)]) -> Harness {
    let mut others: Vec<ActorId> = Vec::new();
    let root = trace.first().map(|(actor, _)| *actor);
    for (actor, _) in trace {
        if Some(*actor) != root && !others.contains(actor) {
            others.push(*actor);
        }
    }
    let slot = |actor: &ActorId| {
        if Some(*actor) == root {
            Some(0)
        } else {
            others
                .iter()
                .position(|other| other == actor)
                .map(|at| at.saturating_add(1))
        }
    };
    let harness = Harness::traced(
        limits,
        trace
            .iter()
            .filter_map(|(actor, observed)| Some((slot(actor)?, observed.event()?.clone()))),
    )
    .with_others(others);
    let resumed = trace.iter().filter_map(|(actor, observed)| match observed {
        Observed::Resumed(records) => Some((*actor, records)),
        Observed::Booted(_)
        | Observed::Decided(..)
        | Observed::Refused
        | Observed::Acked { .. }
        | Observed::Dispatched(_)
        | Observed::Mutated(_)
        | Observed::Ran { .. }
        | Observed::Hung(_)
        | Observed::Read(_) => None,
    });
    let mut born = None;
    let mut loaded = None;
    for (actor, records) in resumed {
        for record in records {
            match record {
                Record::Born { born: who } if Some(actor) == root && born.is_none() => {
                    born = Some(who.clone());
                }
                Record::RulesLoaded { rules, file } if loaded.is_none() => {
                    loaded = Some((rules.clone(), file.clone()));
                }
                Record::Born { .. }
                | Record::RulesLoaded { .. }
                | Record::Delivered { .. }
                | Record::TurnStarted { .. }
                | Record::StepStarted { .. }
                | Record::ModelOutput { .. }
                | Record::StepReset { .. }
                | Record::StepFinished { .. }
                | Record::Approved { .. }
                | Record::Dispatched { .. }
                | Record::ToolResult { .. }
                | Record::TaskEnded { .. }
                | Record::StopSent { .. }
                | Record::CancelRequested { .. }
                | Record::TurnEnded { .. }
                | Record::EpochBumped { .. } => {}
            }
        }
    }
    let harness = match loaded {
        Some((rules, file)) => harness.with_loaded(rules, file),
        None => harness,
    };
    match born {
        Some(born) => harness.with_born(born),
        None => harness,
    }
}
