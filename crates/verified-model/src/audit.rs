//! Facts read back from a log, and the rules it broke.

use std::collections::BTreeSet;

use verified_domain::{
    Abstract, CallId, CallIx, Cause, Exit, Fault, Limits, Record, Request, Rerun, Tool, TurnId,
};

/// Facts read back from the log.
#[derive(Debug, Default)]
pub(crate) struct Audit {
    pub(crate) proposed: BTreeSet<CallId>,
    pub(crate) breaches: BTreeSet<Breach>,
    /// Calls that failed by a limit.
    pub(crate) limited: u8,
    /// A turn was cancelled after its model call finished.
    pub(crate) cancelled_after_finish: Option<TurnId>,
}

/// A rule the log broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Breach {
    /// A call past `calls_per_step` or `calls_per_turn` was dispatched.
    OverBoundDispatched,
    /// An `Unsafe` call dispatched before a crash was resolved after it
    /// other than `Unknown`.
    UnsafeKnownAfterCrash,
}

pub(crate) fn audit(log: &[Record<Abstract>], limits: &Limits) -> Audit {
    let mut audit = Audit::default();
    let mut tools: Vec<Tool> = Vec::new();
    let mut over: BTreeSet<usize> = BTreeSet::new();
    // Unsafe calls dispatched and unresolved, and those of them a crash
    // (an `EpochBumped`) came between dispatch and result for.
    let mut unsafe_live: BTreeSet<usize> = BTreeSet::new();
    let mut crossed: BTreeSet<usize> = BTreeSet::new();
    let mut stopping: BTreeSet<TurnId> = BTreeSet::new();
    for record in log {
        match record {
            Record::TurnStarted { .. } => {
                tools.clear();
                over.clear();
                unsafe_live.clear();
                crossed.clear();
            }
            Record::StepFinished { calls, step } => {
                if stopping.contains(&step.turn) {
                    audit.cancelled_after_finish = Some(step.turn);
                }
                let first = tools.len();
                for proposal in calls {
                    let index = tools.len();
                    if index.saturating_sub(first) >= limits.calls_per_step()
                        || index >= limits.calls_per_turn()
                    {
                        over.insert(index);
                    }
                    tools.push(match &proposal.request {
                        Request::Run { args } => *args,
                        Request::Invalid { .. } => Tool::Read,
                    });
                    audit.proposed.insert(proposal.id.clone());
                }
            }
            Record::CancelRequested { turn } => {
                stopping.insert(*turn);
            }
            Record::Dispatched {
                call: CallIx(index),
            } => {
                if over.contains(index) {
                    audit.breaches.insert(Breach::OverBoundDispatched);
                }
                if tools.get(*index).map(|tool| tool.rerun()) == Some(Rerun::Unsafe) {
                    unsafe_live.insert(*index);
                }
            }
            Record::ToolResult {
                call: CallIx(index),
                exit,
                ..
            } => {
                if *exit == Exit::Err(Cause::Failed(Fault::TooManyCalls)) {
                    audit.limited = audit.limited.saturating_add(1);
                }
                unsafe_live.remove(index);
                if crossed.remove(index) && *exit != Exit::Err(Cause::Unknown) {
                    audit.breaches.insert(Breach::UnsafeKnownAfterCrash);
                }
            }
            Record::EpochBumped { .. } => {
                crossed.extend(unsafe_live.iter().copied());
            }
            Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::Approved { .. }
            | Record::TurnEnded { .. } => {}
        }
    }
    audit
}
