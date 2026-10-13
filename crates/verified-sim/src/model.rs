//! The run as a path of the Stateright model (`verified_model`).
//!
//! The ledger is the run's total order: turmoil runs every host on one
//! thread, and each entry is recorded as it happens, the shell's own
//! observations ([`Entry::Shell`]) on the daemon's task at the point they
//! name. [`observed`] folds it, in α, into the facts the model replays:
//! what each boot recovered, each input entering the core and what it
//! decided, each group commit made durable, each tool run dispatched,
//! acting and returning, and the offset of each record the TUI folded (the
//! offset only: what it folded there is I6's to check). A decision a full
//! log refused is no fact: the core never took its event, so the fold
//! drops it (a refused resume stays, as [`Observed::Refused`]). [`replay`]
//! walks the model along them and checks its properties on every state.

use verified_domain::alpha::record;
use verified_domain::{EffectId, Epoch, Limits, Record};
use verified_model::{Mismatch, Observed, Replayed, alpha};
use verified_server::payload::Parsed;
use verified_server::shim::{Barrier, Observation};

use crate::ledger::{Entry, Stamped};

fn records(records: &[Record<Parsed>]) -> Vec<Record<verified_domain::Abstract>> {
    records.iter().map(record).collect()
}

const fn effect(epoch: u64, seq: u64) -> EffectId {
    EffectId {
        epoch: Epoch(epoch),
        seq,
    }
}

/// The facts of a run's ledger the model replays, in order.
#[must_use]
pub fn observed(entries: &[Stamped]) -> Vec<Observed> {
    let mut trace = Vec::new();
    for stamped in entries {
        match &stamped.entry {
            Entry::Created => trace.push(Observed::Booted(Vec::new())),
            Entry::Recovered { records: found } => trace.push(Observed::Booted(records(found))),
            Entry::Acked {
                first,
                records: synced,
            } => trace.push(Observed::Acked {
                first: *first,
                records: records(synced),
            }),
            Entry::Mutation { effect, .. } => trace.push(Observed::Mutated(*effect)),
            Entry::Read { from, next, .. } => trace.extend((*from..*next).map(Observed::Read)),
            Entry::Shell(Observation::Resumed { records: decided }) => {
                trace.push(Observed::Resumed(records(decided)));
            }
            Entry::Shell(Observation::Decided {
                event,
                records: decided,
            }) => trace.push(Observed::Decided(alpha::event(event), records(decided))),
            Entry::Shell(Observation::Refused) => {
                // What was refused is the last decision reported.
                let last = trace.iter().rposition(|observed| {
                    matches!(observed, Observed::Decided(..) | Observed::Resumed(_))
                });
                match last {
                    Some(at) if matches!(trace.get(at), Some(Observed::Decided(..))) => {
                        trace.remove(at);
                    }
                    Some(_) | None => trace.push(Observed::Refused),
                }
            }
            Entry::Shell(Observation::Passed(Barrier::Dispatched { epoch, seq })) => {
                trace.push(Observed::Dispatched(effect(*epoch, *seq)));
            }
            Entry::Shell(Observation::Passed(Barrier::ToolRan { epoch, seq })) => {
                trace.push(Observed::Ran(effect(*epoch, *seq)));
            }
            Entry::Shell(Observation::Passed(Barrier::GroupWritten { .. }))
            | Entry::Boot { .. }
            | Entry::Wrote { .. }
            | Entry::Fault(_)
            | Entry::User(_)
            | Entry::GaveUp { .. }
            | Entry::Caught { .. } => {}
        }
    }
    trace
}

/// Replays `trace` through the model, under the daemon's core `limits`.
///
/// # Errors
/// The run is not a path of the model, or breaks one of its properties.
pub fn replay(limits: Limits, trace: &[Observed]) -> Result<Replayed, Mismatch> {
    verified_model::replay(&verified_model::replay::harness(limits, trace), trace)
}
