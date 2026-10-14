//! World-level checkers: judge what happened, not what the core claims.
//!
//! Every check reads the [`Ledger`](crate::ledger::Ledger)'s entries: what
//! the environment did to the world, what each actor's log acknowledged as
//! synced, what each recovery found on disk, and what the TUI showed when
//! it was quiescent (every actor quiet).
//!
//! What a tool may do to the world and when (approval and dispatch durable
//! first, an `Unsafe` call at most once), what a crash keeps (every
//! acknowledged record, then a prefix of the pending group), and how a
//! delivery crosses between logs, is the model's replay to judge
//! ([`crate::model`]), from the same ledger.
//!
//! - I5 at quiescence every turn of every actor ended and every call
//!   resolved, unless the actor's log refused an append since the last
//!   boot: storage is the world, and a full log fails stop (L3) with its
//!   turn as it was.
//! - I6 exactly: each stream read the TUI folded, `from..next`, arrived as
//!   the projection of exactly the root's records `from..next`, and the
//!   reads tile the stream from offset 0; once caught up, the TUI's folded
//!   transcript equals the projection of the root's log.
//! - I7 an `edit` the log says succeeded finished a write of its file, and
//!   what it wrote is a version of that file the edit could have read (as
//!   it booted, or as some write before it left it) holding `old` once, with
//!   `old` replaced by `new`: judged from that call's write in the ledger,
//!   since approved edits run at once and one may overwrite another. And
//!   edits are durable: after the final crash each file holds what the last
//!   write to it that finished wrote, or what a write that overlapped that
//!   one wrote (two racing renames land in either order).
//! - I8 each message the user typed is in the root's log exactly once (O2,
//!   L3), but the one it gave up on when the actor took no more, which is
//!   there at most once.
//! - I9 every `send` or `spawn` that resolved `Ok` is in its recipient's
//!   log exactly once at quiescence (O2): its envelope, minted from the
//!   call's key. And every envelope an actor minted names the record of
//!   its log that licensed it: a `send` or `spawn` to that recipient, or a
//!   stop of it (a `Keyed` rerun reuses its key).
//! - I10 every envelope an actor holds (a message, a notification) is
//!   carried by at most one model call, and at quiescence by exactly one,
//!   unless the actor stopped or reached its turn limit (L4).
//! - I11 at quiescence every task ended (H1); each end is its one
//!   notification (H3).
//! - I12 at quiescence every actor a stopped actor spawned is stopped
//!   too (the stop cascade).
//! - I15 every effect with a deadline resolved by it, in simulated time
//!   (B2): its result is durable within its deadline of its `Dispatched`,
//!   unless a crash or a full log came between.
//! - I16 fail-stop (L3): from an actor's refused append until the daemon
//!   boots again, nothing more of its log is acknowledged and none of its
//!   runs or deliveries starts.

use core::fmt;
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};

use verified_domain::{
    ActorId, Address, Body, EffectId, EnvelopeId, Exit, Failure, Limit, Record, Request,
    TurnOutcome, named,
};
use verified_server::limits::ServerLimits;
use verified_server::payload::{Parsed, ToolArgs};
use verified_server::projection::Projector;
use verified_server::shim::{Barrier, Observation};
use verified_wire::WireLimits;
use verified_wire::chunk::UiChunk;
use verified_wire::transcript::Transcript;

use crate::hosts::MODEL;
use crate::ledger::{Entry, Op, Stamped, root};
use crate::scenario::Scenario;

/// A broken invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.invariant, self.detail)
    }
}

const fn violation(invariant: &'static str, detail: String) -> Violation {
    Violation { invariant, detail }
}

/// How long past its deadline an effect's result may take to become
/// durable: the expiry's own decision and group commit, and the
/// scheduling around them.
const DEADLINE_GRACE: Duration = Duration::from_secs(5);

/// One tool call as a log tells it.
#[derive(Debug, Clone)]
struct Call {
    id: String,
    args: Option<ToolArgs>,
    /// The seq of each of its `Dispatched` records: its effects' seqs.
    effects: Vec<u64>,
    results: Vec<(u64, Exit)>,
}

/// The calls of `log`, in order, and any record naming a call its turn did
/// not propose.
fn calls(log: &[Record<Parsed>], found: &mut Vec<Violation>) -> Vec<Call> {
    let mut calls: Vec<Call> = Vec::new();
    for (seq, (record, target)) in (0u64..).zip(named(log)) {
        match record {
            Record::StepFinished {
                calls: proposed, ..
            } => calls.extend(proposed.iter().map(|proposal| Call {
                id: proposal.id.as_str().to_owned(),
                args: match &proposal.request {
                    Request::Run { args } => Some(args.clone()),
                    Request::Invalid { .. } => None,
                },
                effects: Vec::new(),
                results: Vec::new(),
            })),
            Record::Approved { call: ix, .. }
            | Record::Dispatched { call: ix }
            | Record::ToolResult { call: ix, .. } => {
                let Some(call) = target.and_then(|(number, _)| calls.get_mut(number)) else {
                    found.push(violation(
                        "log",
                        format!(
                            "record {seq} names call {}, which its turn never proposed",
                            ix.0
                        ),
                    ));
                    continue;
                };
                match record {
                    Record::Dispatched { .. } => call.effects.push(seq),
                    Record::ToolResult { exit, .. } => call.results.push((seq, *exit)),
                    Record::Approved { .. }
                    | Record::Born { .. }
                    | Record::RulesLoaded { .. }
                    | Record::Delivered { .. }
                    | Record::TaskEnded { .. }
                    | Record::StopSent { .. }
                    | Record::TurnStarted { .. }
                    | Record::StepStarted { .. }
                    | Record::ModelOutput { .. }
                    | Record::StepReset { .. }
                    | Record::StepFinished { .. }
                    | Record::CancelRequested { .. }
                    | Record::TurnEnded { .. }
                    | Record::EpochBumped { .. } => {}
                }
            }
            Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => {}
        }
    }
    calls
}

/// A task of a log: the seq that started it (its id), and whether it is a
/// background call's (its args) or a stop.
fn tasks(log: &[Record<Parsed>]) -> Vec<(u64, Option<ToolArgs>)> {
    let mut started = Vec::new();
    for (seq, (record, call)) in (0u64..).zip(named(log)) {
        match record {
            Record::Dispatched { .. } => {
                if let Some((_, proposal)) = call
                    && let Request::Run { args } = &proposal.request
                    && matches!(
                        args,
                        ToolArgs::Bash {
                            background: true,
                            ..
                        } | ToolArgs::Wake { .. }
                    )
                {
                    started.push((seq, Some(args.clone())));
                }
            }
            Record::StopSent { .. } => started.push((seq, None)),
            Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Approved { .. }
            | Record::ToolResult { .. }
            | Record::TaskEnded { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => {}
        }
    }
    started
}

/// The seq of each `TaskEnded` of `task` in `log`.
fn ends(log: &[Record<Parsed>], task: u64) -> Vec<u64> {
    (0u64..)
        .zip(log)
        .filter(|(_, record)| matches!(record, Record::TaskEnded { task: ended, .. } if ended.0 == task))
        .map(|(seq, _)| seq)
        .collect()
}

/// Who `log`'s actor is, once born.
fn born(log: &[Record<Parsed>]) -> Option<&verified_domain::Born> {
    log.iter().find_map(|record| match record {
        Record::Born { born } => Some(born),
        Record::RulesLoaded { .. }
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
        | Record::EpochBumped { .. } => None,
    })
}

/// Whether `log`'s actor holds a stop.
fn stopped(log: &[Record<Parsed>]) -> bool {
    log.iter().any(|record| {
        matches!(
            record,
            Record::Delivered {
                envelope: verified_domain::Envelope {
                    body: Body::Stop,
                    ..
                }
            }
        )
    })
}

/// I5 over the log of `actor`: every turn ended, every call resolved.
fn quiescent(actor: ActorId, log: &[Record<Parsed>], when: &str, found: &mut Vec<Violation>) {
    let started = log
        .iter()
        .filter(|record| matches!(record, Record::TurnStarted { .. }))
        .count();
    let ended = log
        .iter()
        .filter(|record| matches!(record, Record::TurnEnded { .. }))
        .count();
    if started != ended {
        found.push(violation(
            "I5",
            format!("{when}: {actor} started {started} turns but ended {ended}"),
        ));
    }
    for call in calls(log, found) {
        if call.results.is_empty() {
            found.push(violation(
                "I5",
                format!("{when}: call {} of {actor} has no result", call.id),
            ));
        }
    }
}

/// I10 over the log of `actor`: each envelope it holds carried at most
/// once, and (`settled`) exactly once unless it stopped or reached its
/// turn limit.
fn carried(
    actor: ActorId,
    log: &[Record<Parsed>],
    (settled, turns): (bool, usize),
    found: &mut Vec<Violation>,
) {
    let mut held: Vec<EnvelopeId> = Vec::new();
    let mut steps: BTreeMap<EnvelopeId, usize> = BTreeMap::new();
    for record in log {
        match record {
            Record::Delivered { envelope } if matches!(envelope.body, Body::Text(_)) => {
                held.push(envelope.id);
            }
            Record::TaskEnded { task, .. } => held.push(EnvelopeId::Task(*task)),
            Record::StepStarted { carries, .. } => {
                for id in carries {
                    let count = steps.entry(*id).or_default();
                    *count = count.saturating_add(1);
                }
            }
            Record::Delivered { .. }
            | Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::TurnStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::StopSent { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => {}
        }
    }
    let started = log
        .iter()
        .filter(|record| matches!(record, Record::TurnStarted { .. }))
        .count();
    let context = log.iter().any(|record| {
        matches!(
            record,
            Record::TurnEnded {
                outcome: TurnOutcome::Failed(Failure::Limit(Limit::Context)),
                ..
            }
        )
    });
    let exempt = stopped(log) || started >= turns || context;
    for id in held {
        match steps.get(&id).copied().unwrap_or(0) {
            0 if settled && !exempt => found.push(violation(
                "I10",
                format!("at quiescence no model call of {actor} carried envelope {id:?}"),
            )),
            0 | 1 => {}
            more => found.push(violation(
                "I10",
                format!("{more} model calls of {actor} carried envelope {id:?}"),
            )),
        }
    }
}

/// I11 over the log of `actor` at quiescence: every task ended.
fn tasks_ended(actor: ActorId, log: &[Record<Parsed>], found: &mut Vec<Violation>) {
    for (task, _) in tasks(log) {
        match ends(log, task).len() {
            1 => {}
            count => found.push(violation(
                "I11",
                format!("at quiescence task {task} of {actor} ended {count} times"),
            )),
        }
    }
}

/// I9 over `logs` at quiescence: each `send` or `spawn` that resolved `Ok`
/// is in its recipient's log exactly once.
fn delivered_once(logs: &BTreeMap<ActorId, Vec<Record<Parsed>>>, found: &mut Vec<Violation>) {
    for (sender, log) in logs {
        let mut ignored = Vec::new();
        for call in calls(log, &mut ignored) {
            let (Some(key), true) = (
                call.effects.first(),
                call.results.iter().any(|(_, exit)| *exit == Exit::Ok),
            ) else {
                continue;
            };
            let to = match &call.args {
                Some(ToolArgs::Send {
                    to: Address::Actor(to),
                    ..
                }) => *to,
                Some(ToolArgs::Spawn { child, .. }) => *child,
                Some(_) | None => continue,
            };
            let id = EnvelopeId::Minted {
                actor: *sender,
                seq: *key,
            };
            let landed = logs.get(&to).map_or(0, |log| {
                log.iter()
                    .filter(|record| {
                        matches!(record, Record::Delivered { envelope } if envelope.id == id)
                    })
                    .count()
            });
            if landed != 1 {
                found.push(violation(
                    "I9",
                    format!(
                        "call {} of {sender} delivered Ok, but {to}'s log holds its envelope \
                         {landed} times",
                        call.id
                    ),
                ));
            }
        }
    }
}

/// I9, the other way: every envelope minted by an actor and held by
/// another names the sender's record that licensed it.
fn minted_licensed(logs: &BTreeMap<ActorId, Vec<Record<Parsed>>>, found: &mut Vec<Violation>) {
    let mut licenses: BTreeMap<(ActorId, u64), ActorId> = BTreeMap::new();
    for (sender, log) in logs {
        let mut ignored = Vec::new();
        for call in calls(log, &mut ignored) {
            let to = match &call.args {
                Some(ToolArgs::Send {
                    to: Address::Actor(to),
                    ..
                }) => *to,
                Some(ToolArgs::Spawn { child, .. }) => *child,
                Some(_) | None => continue,
            };
            for seq in &call.effects {
                licenses.insert((*sender, *seq), to);
            }
        }
        for (seq, record) in (0u64..).zip(log) {
            if let Record::StopSent { child } = record {
                licenses.insert((*sender, seq), *child);
            }
        }
    }
    for (recipient, log) in logs {
        for record in log {
            if let Record::Delivered { envelope } = record
                && let EnvelopeId::Minted { actor, seq } = envelope.id
                && licenses.get(&(actor, seq)) != Some(recipient)
            {
                found.push(violation(
                    "I9",
                    format!(
                        "{recipient} holds envelope {:?}, which no record of {actor} licensed \
                         for it",
                        envelope.id
                    ),
                ));
            }
        }
    }
}

/// I12 over `logs` at quiescence: every descendant of a stopped actor is
/// stopped.
fn stops_cascade(logs: &BTreeMap<ActorId, Vec<Record<Parsed>>>, found: &mut Vec<Violation>) {
    let parents: BTreeMap<ActorId, ActorId> = logs
        .values()
        .filter_map(|log| {
            let born = born(log)?;
            Some((born.actor, born.parent?))
        })
        .collect();
    let halted: BTreeSet<ActorId> = logs
        .iter()
        .filter(|(_, log)| stopped(log))
        .map(|(actor, _)| *actor)
        .collect();
    for (child, log) in logs {
        let mut ancestor = parents.get(child).copied();
        let mut hops = 0usize;
        while let Some(parent) = ancestor
            && hops <= logs.len()
        {
            if halted.contains(&parent) && !stopped(log) {
                found.push(violation(
                    "I12",
                    format!("at quiescence {parent} is stopped but its descendant {child} is not"),
                ));
                break;
            }
            ancestor = parents.get(&parent).copied();
            hops = hops.saturating_add(1);
        }
    }
}

/// The transcript a fresh client folds from `log`'s projection.
fn projected(log: &[Record<Parsed>]) -> Result<serde_json::Value, String> {
    let mut projector = Projector::new(MODEL.to_owned());
    let mut transcript = Transcript::new(&WireLimits::DEFAULT);
    for record in log {
        for chunk in projector.project(record) {
            transcript
                .apply(&chunk)
                .map_err(|error| format!("{} does not fold: {error}", chunk.kind()))?;
        }
    }
    serde_json::to_value(transcript.turns()).map_err(|error| error.to_string())
}

/// I6: what the TUI showed is the projection of what the daemon had logged.
fn caught_up(
    log: &[Record<Parsed>],
    offset: u64,
    transcript: &serde_json::Value,
    found: &mut Vec<Violation>,
) {
    let logged = u64::try_from(log.len()).unwrap_or(u64::MAX);
    if offset != logged {
        found.push(violation(
            "I6",
            format!("the quiescent TUI folded up to {offset} but the log holds {logged} records"),
        ));
    }
    let prefix = usize::try_from(offset)
        .ok()
        .and_then(|offset| log.get(..offset))
        .unwrap_or(log);
    match projected(prefix) {
        Ok(expected) if expected == *transcript => {}
        Ok(expected) => found.push(violation(
            "I6",
            format!("the TUI shows {transcript} but the log projects to {expected}"),
        )),
        Err(error) => found.push(violation(
            "I6",
            format!("the log's projection does not fold: {error}"),
        )),
    }
}

/// Each actor's log as a run went: every record with when it became
/// durable (acknowledged, or found at a boot), what the last boot
/// recovered, and the logs at the last quiescence.
#[derive(Debug, Default)]
struct Logs {
    now: BTreeMap<ActorId, Vec<(Duration, Record<Parsed>)>>,
    recovered: BTreeMap<ActorId, Vec<Record<Parsed>>>,
    /// Since the last boot listed the logs.
    listed: bool,
}

impl Logs {
    fn records(&self, actor: ActorId) -> Vec<Record<Parsed>> {
        self.now
            .get(&actor)
            .map(|log| log.iter().map(|(_, record)| record.clone()).collect())
            .unwrap_or_default()
    }

    fn all(&self) -> BTreeMap<ActorId, Vec<Record<Parsed>>> {
        self.now
            .keys()
            .map(|actor| (*actor, self.records(*actor)))
            .collect()
    }
}

/// Every check, over a finished run's ledger.
#[must_use]
pub fn world(entries: &[Stamped], scenario: &Scenario) -> Vec<Violation> {
    let mut found = Vec::new();
    let root = root(entries);
    let mut logs = Logs::default();
    let mut reads: Vec<Read<'_>> = Vec::new();
    let mut disk = Disk::default();
    let mut caught: Option<BTreeMap<ActorId, Vec<Record<Parsed>>>> = None;
    let mut gave_up = false;
    let mut typed = 0usize;
    // Actors whose log refused an append since the last boot, and when.
    let mut halted: BTreeMap<ActorId, Duration> = BTreeMap::new();
    let mut refusals: Vec<(ActorId, Duration)> = Vec::new();
    let mut boots: Vec<Duration> = Vec::new();
    for (at, stamped) in entries.iter().enumerate() {
        match &stamped.entry {
            Entry::Listed { .. } => {
                logs.listed = true;
                logs.recovered.clear();
            }
            Entry::Created { actor } => {
                logs.now.insert(*actor, Vec::new());
            }
            Entry::Recovered { actor, records } => {
                let log = logs.now.entry(*actor).or_default();
                let kept: Vec<(Duration, Record<Parsed>)> = records
                    .iter()
                    .enumerate()
                    .map(|(index, record)| {
                        let when = log
                            .get(index)
                            .filter(|(_, was)| was == record)
                            .map_or(stamped.at, |(when, _)| *when);
                        (when, record.clone())
                    })
                    .collect();
                *log = kept;
                if logs.listed {
                    logs.recovered.insert(*actor, records.clone());
                }
            }
            Entry::Acked { actor, records, .. } => {
                if halted.contains_key(actor) {
                    found.push(violation(
                        "I16",
                        format!(
                            "{actor} acknowledged {} records at {:?}, after its log refused an \
                             append",
                            records.len(),
                            stamped.at
                        ),
                    ));
                }
                logs.now
                    .entry(*actor)
                    .or_default()
                    .extend(records.iter().map(|record| (stamped.at, record.clone())));
            }
            Entry::Caught { offset, transcript } => {
                for (actor, log) in logs.all() {
                    if !halted.contains_key(&actor) {
                        quiescent(actor, &log, "at quiescence", &mut found);
                    }
                }
                if let Some(root) = root {
                    caught_up(&logs.records(root), *offset, transcript, &mut found);
                }
                caught = Some(logs.all());
            }
            Entry::Boot { files, .. } => {
                halted.clear();
                boots.push(stamped.at);
                disk.boot(at, files);
            }
            Entry::Shell(Observation::Refused { actor }) => {
                halted.entry(*actor).or_insert(stamped.at);
                refusals.push((*actor, stamped.at));
            }
            Entry::Shell(Observation::Passed(
                Barrier::Dispatched { actor, .. } | Barrier::Delivering { actor, .. },
            )) if halted.contains_key(actor) => found.push(violation(
                "I16",
                format!(
                    "{actor} started an effect at {:?}, after its log refused an append",
                    stamped.at
                ),
            )),
            Entry::Wrote { actor, effect } => disk.wrote(at, (*actor, *effect), &mut found),
            Entry::Mutation {
                actor,
                call,
                effect,
                op,
            } => {
                if let Op::Write { path, text } = op {
                    disk.writes.push(Write {
                        actor: *actor,
                        call: call.as_deref(),
                        effect: *effect,
                        path,
                        text,
                        start: at,
                        done: None,
                        end: None,
                    });
                }
            }
            Entry::Read { from, next, chunks } => reads.push(Read {
                from: *from,
                next: *next,
                chunks,
            }),
            Entry::GaveUp { .. } => gave_up = true,
            Entry::User(done) => {
                if done.starts_with("send ") || done.starts_with("steer ") {
                    typed = typed.saturating_add(1);
                }
            }
            Entry::Fault(_) | Entry::Shell(_) => {}
        }
    }
    let end = entries.last().map_or(Duration::ZERO, |stamped| stamped.at);
    deadlines(
        &logs,
        &scenario.limits,
        (&boots, &refusals, end),
        &mut found,
    );
    let Some(root) = root else {
        found.push(violation(
            "harness",
            "the daemon never created the root's log".to_owned(),
        ));
        return found;
    };
    match &caught {
        None => found.push(violation(
            "I5",
            "the TUI never reached quiescence".to_owned(),
        )),
        Some(quiet) => {
            let turns = scenario.limits.core.turns_per_actor();
            for (actor, log) in quiet {
                let settled = !halted.contains_key(actor);
                carried(*actor, log, (settled, turns), &mut found);
                if settled {
                    tasks_ended(*actor, log, &mut found);
                }
            }
            delivered_once(quiet, &mut found);
            minted_licensed(quiet, &mut found);
            stops_cascade(quiet, &mut found);
        }
    }
    let Some(last) = logs.recovered.get(&root).cloned() else {
        found.push(violation(
            "harness",
            "the daemon never recovered the root's log".to_owned(),
        ));
        return found;
    };
    if let Some(recovered_at) = entries
        .iter()
        .rposition(|stamped| matches!(stamped.entry, Entry::Listed { .. }))
        && entries
            .get(recovered_at..)
            .unwrap_or_default()
            .iter()
            .any(|stamped| matches!(stamped.entry, Entry::Caught { .. }))
    {
        found.push(violation(
            "harness",
            "the final recovery came before the TUI was quiescent".to_owned(),
        ));
    }
    messages_delivered(&last, (typed, gave_up), &mut found);
    for (actor, log) in &logs.recovered {
        edits_applied(*actor, log, &disk, &mut found);
    }
    edits_durable(&disk, &mut found);
    reads_exact(&last, &reads, &mut found);
    found
}

/// I15: every effect with a deadline resolved within it of its
/// `Dispatched` becoming durable, unless a boot (a crash) or a refused
/// append of its actor came between; one still open at `end` counts too.
fn deadlines(
    logs: &Logs,
    limits: &ServerLimits,
    (boots, refusals, end): (&[Duration], &[(ActorId, Duration)], Duration),
    found: &mut Vec<Violation>,
) {
    for (actor, timed) in &logs.now {
        let log: Vec<Record<Parsed>> = timed.iter().map(|(_, record)| record.clone()).collect();
        let when = |seq: u64| {
            usize::try_from(seq)
                .ok()
                .and_then(|at| timed.get(at))
                .map(|(when, _)| *when)
        };
        let mut ignored = Vec::new();
        let mut open: Vec<(String, u64, Duration, Option<u64>)> = Vec::new();
        for call in calls(&log, &mut ignored) {
            let (Some(args), Some(dispatched)) = (&call.args, call.effects.first()) else {
                continue;
            };
            let background = matches!(
                args,
                ToolArgs::Bash {
                    background: true,
                    ..
                } | ToolArgs::Wake { .. }
            );
            let Some(deadline) = limits.deadline(args) else {
                continue;
            };
            let resolved = if background {
                ends(&log, *dispatched).first().copied()
            } else {
                call.results.first().map(|(seq, _)| *seq)
            };
            open.push((
                call.id.clone(),
                *dispatched,
                Duration::from_millis(deadline.get()),
                resolved,
            ));
        }
        for (task, args) in tasks(&log) {
            if args.is_none() {
                open.push((
                    format!("stop task {task}"),
                    task,
                    Duration::from_millis(limits.deliver_deadline.get()),
                    ends(&log, task).first().copied(),
                ));
            }
        }
        for (what, dispatched, deadline, resolved) in open {
            let Some(from) = when(dispatched) else {
                continue;
            };
            let due = from.saturating_add(deadline).saturating_add(DEADLINE_GRACE);
            let to = resolved.and_then(when).unwrap_or(end);
            if to <= due {
                continue;
            }
            let crashed = boots.iter().any(|boot| *boot > from && *boot <= due);
            let refused = refusals
                .iter()
                .any(|(of, at)| of == actor && *at >= from && *at <= due);
            if !crashed && !refused {
                found.push(violation(
                    "I15",
                    format!(
                        "{what} of {actor}, dispatched at {from:?} with a deadline of \
                         {deadline:?}, {}",
                        if resolved.is_some() {
                            format!("resolved only at {to:?}")
                        } else {
                            format!("was still open at {end:?}")
                        }
                    ),
                ));
            }
        }
    }
}

/// One `edit`'s write, as the ledger tells it.
#[derive(Debug, Clone, Copy)]
struct Write<'ledger> {
    actor: ActorId,
    call: Option<&'ledger str>,
    effect: EffectId,
    path: &'ledger str,
    text: &'ledger str,
    /// The ledger index it started at.
    start: usize,
    /// The ledger index it returned at, if it did.
    done: Option<usize>,
    /// The ledger index a crash cut it off at (the next boot), if one did.
    end: Option<usize>,
}

impl Write<'_> {
    /// Whether this write may still have been landing at ledger index `at`.
    fn open_at(&self, at: usize) -> bool {
        self.done.or(self.end).is_none_or(|end| end > at)
    }
}

/// The project's files as each boot found them, and every edit's write.
#[derive(Debug, Default)]
struct Disk<'ledger> {
    boots: Vec<(usize, &'ledger BTreeMap<String, String>)>,
    writes: Vec<Write<'ledger>>,
}

impl<'ledger> Disk<'ledger> {
    /// A boot at ledger index `at` found `files`; it cut off every write
    /// still running.
    fn boot(&mut self, at: usize, files: &'ledger BTreeMap<String, String>) {
        for write in &mut self.writes {
            if write.done.is_none() && write.end.is_none() {
                write.end = Some(at);
            }
        }
        self.boots.push((at, files));
    }

    /// The write of `effect` of an actor returned at ledger index `at`.
    fn wrote(
        &mut self,
        at: usize,
        (actor, effect): (ActorId, EffectId),
        found: &mut Vec<Violation>,
    ) {
        let running = self.writes.iter_mut().rev().find(|write| {
            write.actor == actor
                && write.effect == effect
                && write.done.is_none()
                && write.end.is_none()
        });
        match running {
            Some(write) => write.done = Some(at),
            None => found.push(violation(
                "harness",
                format!("the write of effect {effect} of {actor} returned, but none was running"),
            )),
        }
    }

    /// Every text `path` could hold before ledger index `at`: as some boot
    /// found it, or as some write that started earlier left it.
    fn versions<'disk>(
        &'disk self,
        path: &'disk str,
        at: usize,
    ) -> impl Iterator<Item = &'ledger str> + 'disk {
        let booted = self
            .boots
            .iter()
            .filter(move |(boot, _)| *boot < at)
            .filter_map(move |(_, files)| files.get(path).map(String::as_str));
        let written = self
            .writes
            .iter()
            .filter(move |write| write.path == path && write.start < at)
            .map(|write| write.text);
        booted.chain(written)
    }
}

/// I7, edit by edit: each edit the final log of `actor` says succeeded
/// finished a write that is an earlier version of its file with `old`
/// replaced by `new`.
fn edits_applied(
    actor: ActorId,
    log: &[Record<Parsed>],
    disk: &Disk<'_>,
    found: &mut Vec<Violation>,
) {
    let mut ignored = Vec::new();
    for call in calls(log, &mut ignored) {
        let Some(ToolArgs::Edit { path, old, new }) = &call.args else {
            continue;
        };
        if !matches!(call.results.as_slice(), [(_, Exit::Ok)]) {
            continue;
        }
        let path = path.as_str();
        let finished: Vec<&Write<'_>> = disk
            .writes
            .iter()
            .filter(|write| {
                write.actor == actor
                    && write.call == Some(call.id.as_str())
                    && call.effects.contains(&write.effect.seq)
                    && write.path == path
                    && write.done.is_some()
            })
            .collect();
        if finished.is_empty() {
            found.push(violation(
                "I7",
                format!(
                    "the log says edit {} put {new:?} in {path}, but no write of it finished",
                    call.id
                ),
            ));
            continue;
        }
        let applied = finished.iter().any(|write| {
            disk.versions(path, write.start).any(|before| {
                before.match_indices(old.as_str()).take(2).count() == 1
                    && before.replacen(old.as_str(), new, 1) == write.text
            })
        });
        if !applied {
            let wrote: Vec<&str> = finished.iter().map(|write| write.text).collect();
            found.push(violation(
                "I7",
                format!(
                    "edit {} wrote {wrote:?} to {path}, which is no earlier version of it with {old:?} replaced by {new:?}",
                    call.id
                ),
            ));
        }
    }
}

/// I7, durably: after the final crash each file holds what the last write
/// to it that finished wrote (or, with none, what it held at first), or what
/// a write still open when that one started wrote.
fn edits_durable(disk: &Disk<'_>, found: &mut Vec<Violation>) {
    let (Some((_, first)), Some((last_boot, last))) = (disk.boots.first(), disk.boots.last())
    else {
        return;
    };
    for write in &disk.writes {
        if !last.contains_key(write.path) {
            found.push(violation(
                "harness",
                format!("an edit wrote {}, which no boot reads back", write.path),
            ));
        }
    }
    for (path, holds) in *last {
        let writes: Vec<&Write<'_>> = disk
            .writes
            .iter()
            .filter(|write| write.path == path && write.start < *last_boot)
            .collect();
        let finished = writes
            .iter()
            .filter_map(|write| write.done.map(|done| (done, *write)))
            .max_by_key(|(done, _)| *done)
            .map(|(_, write)| write);
        let mut allowed: Vec<&str> = match finished {
            Some(landed) => vec![landed.text],
            None => first.get(path).map(String::as_str).into_iter().collect(),
        };
        let since = finished.map_or(0, |landed| landed.start);
        allowed.extend(
            writes
                .iter()
                .filter(|write| write.open_at(since))
                .map(|write| write.text),
        );
        if !allowed.contains(&holds.as_str()) {
            found.push(violation(
                "I7",
                format!(
                    "after the final crash {path} holds {holds:?}, but the last finished write to it left {:?}",
                    allowed.first().copied().unwrap_or_default()
                ),
            ));
        }
    }
}

/// One stream read the TUI folded.
#[derive(Debug, Clone, Copy)]
struct Read<'ledger> {
    from: u64,
    next: u64,
    chunks: &'ledger [UiChunk],
}

/// I6, read by read: the TUI's reads tile the stream from 0, and each
/// carried exactly the projection of the log's records it covers. Reports
/// the first read that does not.
fn reads_exact(log: &[Record<Parsed>], reads: &[Read<'_>], found: &mut Vec<Violation>) {
    let mut projector = Projector::new(MODEL.to_owned());
    let per_record: Vec<Vec<UiChunk>> =
        log.iter().map(|record| projector.project(record)).collect();
    let mut expected_from = 0u64;
    for (index, read) in reads.iter().enumerate() {
        if read.from != expected_from {
            found.push(violation(
                "I6",
                format!(
                    "read {index} of the TUI started at offset {}, but the reads before it ended at {expected_from}",
                    read.from
                ),
            ));
            return;
        }
        let covered = usize::try_from(read.from)
            .ok()
            .zip(usize::try_from(read.next).ok())
            .and_then(|(from, next)| per_record.get(from..next));
        let Some(covered) = covered else {
            found.push(violation(
                "I6",
                format!(
                    "read {index} of the TUI covers offsets {}..{}, past the {} records of the final log",
                    read.from,
                    read.next,
                    log.len()
                ),
            ));
            return;
        };
        let expected: Vec<&UiChunk> = covered.iter().flatten().collect();
        let received: Vec<&UiChunk> = read.chunks.iter().collect();
        if expected != received {
            let kinds = |chunks: &[&UiChunk]| -> Vec<&'static str> {
                chunks.iter().map(|chunk| chunk.kind()).collect()
            };
            found.push(violation(
                "I6",
                format!(
                    "read {index} of the TUI, offsets {}..{}, arrived as {:?} but the log projects them to {:?}",
                    read.from,
                    read.next,
                    kinds(&received),
                    kinds(&expected)
                ),
            ));
            return;
        }
        expected_from = read.next;
    }
}

/// I8 over the root's log the final recovery found: each of the `typed`
/// messages is in it exactly once, but the last when the user gave up.
fn messages_delivered(
    log: &[Record<Parsed>],
    (typed, gave_up): (usize, bool),
    found: &mut Vec<Violation>,
) {
    let mut seen: BTreeMap<EnvelopeId, usize> = BTreeMap::new();
    for record in log {
        if let Record::Delivered { envelope } = record
            && envelope.from == Address::User
            && matches!(envelope.body, Body::Text(_))
        {
            let count = seen.entry(envelope.id).or_default();
            *count = count.saturating_add(1);
        }
    }
    if let Some((id, count)) = seen.iter().find(|(_, count)| **count > 1) {
        found.push(violation(
            "I8",
            format!("the user's message {id:?} is in the root's log {count} times"),
        ));
    }
    let delivered = seen.len();
    let expected = if gave_up {
        delivered == typed || delivered.saturating_add(1) == typed
    } else {
        delivered == typed
    };
    if !expected {
        found.push(violation(
            "I8",
            format!(
                "the user typed {typed} messages (gave up: {gave_up}); the root's log holds \
                 {delivered}"
            ),
        ));
    }
}
