//! Facts read back from a log, and the rules it broke. The audit reads the
//! log itself, never the core: it is a second opinion on what `spec_apply`
//! refuses, and the model's properties stand on it.

use std::collections::{BTreeMap, BTreeSet};

use verified_core::rules::rule_fits;
use verified_domain::{
    Abstract, ActorName, Body, CallId, CallIx, Class, EnvelopeId, Exit, Failure, Grant, Limit,
    Limits, Payload as _, Proposal, Record, Request, Rules, TaskId, TurnId, TurnOutcome, Unstarted,
    named,
};

/// Facts read back from the log.
#[derive(Debug, Default)]
pub(crate) struct Audit {
    pub(crate) proposed: BTreeSet<CallId>,
    /// Calls granted in the user's name (`Grant::User`).
    pub(crate) user_grants: BTreeSet<CallId>,
    pub(crate) breaches: BTreeSet<Breach>,
    /// Calls that failed by a limit.
    pub(crate) limited: u8,
    /// A turn was cancelled after its model call finished.
    pub(crate) cancelled_after_finish: Option<TurnId>,
    /// Every envelope that entered the mailbox, in order: each text
    /// delivered, and each task's end (its notification).
    pub(crate) mail: Vec<EnvelopeId>,
    /// Every envelope a step carried.
    pub(crate) carried: BTreeSet<EnvelopeId>,
    /// A step carried more than one envelope that was not the user's.
    pub(crate) coalesced: bool,
    /// Every envelope appended, stops included.
    pub(crate) delivered: BTreeSet<EnvelopeId>,
    /// A stop landed: nothing more starts for the actor.
    pub(crate) stopped: bool,
    pub(crate) turns: usize,
    /// A turn ended at the context bound: the mail after it may stay
    /// unread (T3 and P3 have yet to decide what a full context does).
    pub(crate) context_full: bool,
    /// Each task by its seq, and whether it ended.
    pub(crate) tasks: BTreeMap<u64, bool>,
    /// The seqs of the records that license a delivery: a `Keyed` call's
    /// `Dispatched` and a `StopSent`.
    pub(crate) licenses: BTreeSet<u64>,
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
    /// call (C1). α keeps no subject, so a pattern is matched against an
    /// empty one: what the model checks of patterns is only `Any`'s.
    GrantUnfit,
    /// A call or a task resolved a second time, or a task never started
    /// ended (T2).
    ResolvedTwice,
    /// A step carried an envelope a step had carried already, or one not
    /// in the mailbox (T3).
    CarriedTwice,
    /// A step carried mail while a call of the step before it was still
    /// unresolved: an injection between a call and its result (T3).
    Spliced,
    /// The actor went past one of its lifetime caps: turns, messages,
    /// tasks, depth (P3).
    OverLimit,
}

#[expect(
    clippy::too_many_lines,
    reason = "one walk over the log, one arm per record"
)]
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
    // Calls resolved, by number, and calls of the turn not yet resolved,
    // by index; whether the running turn has started a step yet.
    let mut resolved: BTreeSet<usize> = BTreeSet::new();
    let mut unresolved: BTreeSet<usize> = BTreeSet::new();
    let mut stepped = true;
    let mut messages = 0usize;
    for (seq, (record, target)) in (0u64..).zip(named(log)) {
        match record {
            Record::Born { born } => {
                name = Some(&born.name);
                if born.depth > limits.depth() {
                    audit.breaches.insert(Breach::OverLimit);
                }
            }
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
            Record::Approved {
                by: Grant::User, ..
            } => {
                if let Some((_, proposal)) = target {
                    audit.user_grants.insert(proposal.id.clone());
                }
            }
            Record::Delivered { envelope } => {
                audit.delivered.insert(envelope.id);
                match envelope.body {
                    Body::Text(_) => {
                        audit.mail.push(envelope.id);
                        messages = messages.saturating_add(1);
                        if messages > limits.messages_per_actor() {
                            audit.breaches.insert(Breach::OverLimit);
                        }
                    }
                    Body::Stop => audit.stopped = true,
                }
            }
            Record::TurnStarted { .. } => {
                proposed = 0;
                over.clear();
                unresolved.clear();
                stepped = false;
                audit.turns = audit.turns.saturating_add(1);
                if audit.turns > limits.turns_per_actor() {
                    audit.breaches.insert(Breach::OverLimit);
                }
            }
            Record::StepStarted { carries, .. } => {
                if !carries.is_empty() && stepped && !unresolved.is_empty() {
                    audit.breaches.insert(Breach::Spliced);
                }
                stepped = true;
                let waiting: BTreeSet<&EnvelopeId> = audit.mail.iter().collect();
                for id in carries {
                    if !waiting.contains(id) || !audit.carried.insert(*id) {
                        audit.breaches.insert(Breach::CarriedTwice);
                    }
                }
                let notifications = carries
                    .iter()
                    .filter(|id| !matches!(id, EnvelopeId::User(_)))
                    .count();
                if notifications >= 2 {
                    audit.coalesced = true;
                }
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
                    unresolved.insert(proposed);
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
                if let Some((number, proposal)) = target {
                    match shape(proposal) {
                        Some((Class::Unsafe, false)) => {
                            unsafe_live.insert(number);
                        }
                        Some((Class::Keyed, _)) => {
                            audit.licenses.insert(seq);
                        }
                        Some(_) | None => {}
                    }
                    if shape(proposal).is_some_and(|(_, background)| background) {
                        audit.tasks.insert(seq, false);
                        if audit.tasks.len() > limits.tasks_per_actor() {
                            audit.breaches.insert(Breach::OverLimit);
                        }
                    }
                }
            }
            Record::ToolResult {
                call: CallIx(index),
                exit,
                ..
            } => {
                if *exit == Exit::NotStarted(Unstarted::Limit) {
                    audit.limited = audit.limited.saturating_add(1);
                }
                unresolved.remove(index);
                if let Some((number, _)) = target {
                    unsafe_live.remove(&number);
                    if !resolved.insert(number) {
                        audit.breaches.insert(Breach::ResolvedTwice);
                    }
                    if crossed.remove(&number) && *exit != Exit::Unknown {
                        audit.breaches.insert(Breach::UnsafeKnownAfterCrash);
                    }
                }
            }
            Record::TaskEnded {
                task: TaskId(task), ..
            } => {
                match audit.tasks.get_mut(task) {
                    Some(ended) if !*ended => *ended = true,
                    Some(_) | None => {
                        audit.breaches.insert(Breach::ResolvedTwice);
                    }
                }
                audit.mail.push(EnvelopeId::Task(TaskId(*task)));
            }
            Record::StopSent { .. } => {
                audit.licenses.insert(seq);
                audit.tasks.insert(seq, false);
            }
            Record::EpochBumped { .. } => {
                crossed.extend(unsafe_live.iter().copied());
            }
            Record::TurnEnded { outcome, .. } => {
                if *outcome == TurnOutcome::Failed(Failure::Limit(Limit::Context)) {
                    audit.context_full = true;
                }
            }
            Record::ModelOutput { .. } | Record::StepReset { .. } => {}
        }
    }
    audit
}

impl Audit {
    /// Whether all the actor's mail has been carried, unless it stopped or
    /// reached its turn cap or a full context (T3's exceptions).
    pub(crate) fn read_all(&self, limits: &Limits) -> bool {
        self.stopped
            || self.context_full
            || self.turns >= limits.turns_per_actor()
            || self.mail.iter().all(|id| self.carried.contains(id))
    }

    /// The tasks still running.
    pub(crate) fn running(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.tasks
            .iter()
            .filter(|(_, ended)| !**ended)
            .map(|(seq, _)| TaskId(*seq))
    }
}

/// The class of the tool `proposal` asks for and whether it starts a task,
/// if it asks for one.
const fn shape(proposal: &Proposal<Abstract>) -> Option<(Class, bool)> {
    match &proposal.request {
        Request::Run { args } => Some((args.tool().class(), args.background())),
        Request::Invalid { .. } => None,
    }
}
