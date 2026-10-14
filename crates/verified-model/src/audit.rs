//! Facts read back from a log, and the rules it broke.

use std::collections::BTreeSet;

use verified_core::rules::rule_fits;
use verified_domain::{
    Abstract, ActorName, CallId, CallIx, Class, Exit, Grant, Limits, Payload as _, Proposal,
    Record, Request, Rules, TurnId, Unstarted, named,
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
    /// An `Approved` names a rule that is not one of the rules in force
    /// (by the seq of their `RulesLoaded`), or one that does not fit the
    /// call (A2). α keeps no subject, so a pattern is matched against an
    /// empty one: what the model checks of patterns is only `Any`'s.
    GrantUnfit,
}

pub(crate) fn audit(log: &[Record<Abstract>], limits: &Limits) -> Audit {
    let mut audit = Audit::default();
    // The turn's calls so far, and those of them past a bound.
    let mut proposed = 0usize;
    let mut over: BTreeSet<usize> = BTreeSet::new();
    // Unsafe calls dispatched and unresolved, and those of them a crash
    // (an `EpochBumped`) came between dispatch and result for, by number.
    let mut unsafe_live: BTreeSet<usize> = BTreeSet::new();
    let mut crossed: BTreeSet<usize> = BTreeSet::new();
    let mut stopping: BTreeSet<TurnId> = BTreeSet::new();
    let mut name: Option<&ActorName> = None;
    let mut rules: Option<(u64, &Rules)> = None;
    for (seq, (record, target)) in (0u64..).zip(named(log)) {
        match record {
            Record::Born { born } => name = Some(&born.name),
            Record::RulesLoaded { rules: loaded, .. } => rules = Some((seq, loaded)),
            Record::Approved {
                by:
                    Grant::Rule {
                        rule,
                        rules: pinned,
                    },
                ..
            } => {
                let fits = rules.is_some_and(|(at, in_force)| {
                    at == *pinned
                        && usize::try_from(*rule)
                            .ok()
                            .and_then(|rule| in_force.rules.get(rule))
                            .zip(target)
                            .is_some_and(|(rule, (_, proposal))| match &proposal.request {
                                Request::Run { args } => {
                                    rule_fits(rule, name, &Abstract::shape(args))
                                }
                                Request::Invalid { .. } => false,
                            })
                });
                if !fits {
                    audit.breaches.insert(Breach::GrantUnfit);
                }
            }
            Record::TurnStarted { .. } => {
                proposed = 0;
                over.clear();
            }
            Record::StepFinished { calls, step } => {
                if stopping.contains(&step.turn) {
                    audit.cancelled_after_finish = Some(step.turn);
                }
                let first = proposed;
                for proposal in calls {
                    if proposed.saturating_sub(first) >= limits.calls_per_step()
                        || proposed >= limits.calls_per_turn()
                    {
                        over.insert(proposed);
                    }
                    proposed = proposed.saturating_add(1);
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
                if let Some((number, proposal)) = target
                    && class(proposal) == Some(Class::Unsafe)
                {
                    unsafe_live.insert(number);
                }
            }
            Record::ToolResult { exit, .. } => {
                if *exit == Exit::NotStarted(Unstarted::Limit) {
                    audit.limited = audit.limited.saturating_add(1);
                }
                if let Some((number, _)) = target {
                    unsafe_live.remove(&number);
                    if crossed.remove(&number) && *exit != Exit::Unknown {
                        audit.breaches.insert(Breach::UnsafeKnownAfterCrash);
                    }
                }
            }
            Record::EpochBumped { .. } => {
                crossed.extend(unsafe_live.iter().copied());
            }
            Record::StepStarted { .. }
            | Record::Delivered { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::Approved { .. }
            | Record::TurnEnded { .. } => {}
        }
    }
    audit
}

/// The class of the tool `proposal` asks for, if it asks for one.
const fn class(proposal: &Proposal<Abstract>) -> Option<Class> {
    match &proposal.request {
        Request::Run { args } => Some(args.tool().class()),
        Request::Invalid { .. } => None,
    }
}
