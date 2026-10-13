//! World-level checkers: judge what happened, not what the core claims.
//!
//! Every check reads the [`Ledger`](crate::ledger::Ledger)'s entries: what
//! the environment did to the world, what the log acknowledged as synced,
//! what each recovery found on disk, and what the TUI showed when it was
//! quiescent.
//!
//! - I1 every world mutation carried an approval, durably and beforehand:
//!   when the environment changed the world for a call, the log this daemon
//!   life knew to be synced (what it recovered plus every group acknowledged
//!   since) already held the call's `Approved`, and the very `Dispatched`
//!   record its effect id names (by seq), for a tool that makes that kind
//!   of change, and the effect ran in that log's current epoch (the one its
//!   last `EpochBumped` set: a run started before its decision's epoch was
//!   durable, or left over from an earlier life, does not match); and the
//!   final log still holds them.
//! - I2 every call has exactly one `ToolResult` in the final log.
//! - I3 an Unsafe call (`edit`, `bash`) acted on the world at most once,
//!   across every crash.
//! - I4 every recovered log extends every record acknowledged as synced.
//! - I5 at quiescence every turn ended and every call resolved.
//! - I6 exactly: each stream read the TUI folded, `from..next`, arrived as
//!   the projection of exactly the log's records `from..next`, and the reads
//!   tile the stream from offset 0; once caught up, the TUI's folded
//!   transcript equals the projection of the daemon's log.
//! - I7 an `edit` the log says succeeded finished a write of its file, and
//!   what it wrote is a version of that file the edit could have read (as
//!   it booted, or as some write before it left it) holding `old` once, with
//!   `old` replaced by `new`: judged from that call's write in the ledger,
//!   since approved edits run at once and one may overwrite another. And
//!   edits are durable: after the final crash each file holds what the last
//!   write to it that finished wrote, or what a write that overlapped that
//!   one wrote (two racing renames land in either order).
//! - I8 each message the user sent started exactly one turn.

use core::fmt;
use std::collections::BTreeMap;

use verified_domain::{EffectId, Exit, Record, Request, Rerun, Tool};
use verified_server::payload::{Parsed, ToolArgs};
use verified_server::projection::Projector;
use verified_wire::WireLimits;
use verified_wire::chunk::UiChunk;
use verified_wire::transcript::Transcript;

use crate::hosts::MODEL;
use crate::ledger::{Entry, Op, Stamped};
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

/// One tool call as a log tells it.
#[derive(Debug, Clone)]
struct Call {
    turn: usize,
    id: String,
    tool: Option<Tool>,
    args: Option<ToolArgs>,
    approved: usize,
    dispatched: usize,
    /// The seq of each of its `Dispatched` records: its effects' seqs.
    effects: Vec<u64>,
    results: Vec<Exit>,
}

/// The calls of `log`, in order, and any record naming a call that is not
/// there.
fn calls(log: &[Record<Parsed>], found: &mut Vec<Violation>) -> Vec<Call> {
    let mut calls: Vec<Call> = Vec::new();
    let mut base = 0usize;
    let mut turn = 0usize;
    for (seq, record) in (0u64..).zip(log) {
        let ix = match record {
            Record::TurnStarted { .. } => {
                base = calls.len();
                turn = turn.saturating_add(1);
                None
            }
            Record::StepFinished {
                calls: proposed, ..
            } => {
                for proposal in proposed {
                    let (tool, args) = match &proposal.request {
                        Request::Run { args } => (Some(args.tool()), Some(args.clone())),
                        Request::Invalid { .. } => (None, None),
                    };
                    calls.push(Call {
                        turn,
                        id: proposal.id.as_str().to_owned(),
                        tool,
                        args,
                        approved: 0,
                        dispatched: 0,
                        effects: Vec::new(),
                        results: Vec::new(),
                    });
                }
                None
            }
            Record::Approved { call, .. }
            | Record::Dispatched { call }
            | Record::ToolResult { call, .. } => Some(call.0),
            Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        };
        let Some(ix) = ix else { continue };
        let Some(call) = base
            .checked_add(ix)
            .and_then(|at| calls.get_mut(at))
            .filter(|call| call.turn == turn)
        else {
            found.push(violation(
                "log",
                format!("a record names call {ix} of turn {turn}, which was never proposed"),
            ));
            continue;
        };
        match record {
            Record::Approved { .. } => call.approved = call.approved.saturating_add(1),
            Record::Dispatched { .. } => {
                if call.approved == 0 {
                    found.push(violation(
                        "I1",
                        format!("call {} was dispatched before it was approved", call.id),
                    ));
                }
                call.dispatched = call.dispatched.saturating_add(1);
                call.effects.push(seq);
            }
            Record::ToolResult { exit, .. } => call.results.push(*exit),
            Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => {}
        }
    }
    calls
}

/// I5 over `log`: every turn ended, every call resolved.
fn quiescent(log: &[Record<Parsed>], when: &str, found: &mut Vec<Violation>) {
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
            format!("{when}: {started} turns started but {ended} ended"),
        ));
    }
    for call in calls(log, found) {
        if call.results.is_empty() {
            found.push(violation(
                "I5",
                format!("{when}: call {} has no result", call.id),
            ));
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

/// Every check, over a finished run's ledger.
#[must_use]
pub fn world(entries: &[Stamped], scenario: &Scenario) -> Vec<Violation> {
    let mut found = Vec::new();
    let mut log: Vec<Record<Parsed>> = Vec::new();
    let mut last_recovered: Option<(usize, Vec<Record<Parsed>>)> = None;
    let mut mutations: Vec<(Option<String>, Op)> = Vec::new();
    let mut reads: Vec<Read<'_>> = Vec::new();
    let mut disk = Disk::default();
    // Whether this daemon life has opened or created its log yet: until it
    // has, it knows nothing durable, and no tool of it may run.
    let mut known = false;
    let mut caught = 0usize;
    for (at, stamped) in entries.iter().enumerate() {
        match &stamped.entry {
            Entry::Created => {
                if !log.is_empty() {
                    found.push(violation(
                        "I4",
                        "a session was created over a non-empty log".to_owned(),
                    ));
                }
                log.clear();
                known = true;
            }
            Entry::Recovered { records } => {
                let kept = records.get(..log.len()).is_some_and(|kept| kept == log);
                if !kept {
                    found.push(violation(
                        "I4",
                        format!(
                            "recovery at {}ms found {} records that do not extend the {} acknowledged",
                            stamped.at.as_millis(),
                            records.len(),
                            log.len()
                        ),
                    ));
                }
                log.clone_from(records);
                known = true;
                last_recovered = Some((at, records.clone()));
            }
            Entry::Acked { first, records } => {
                let next = u64::try_from(log.len()).unwrap_or(u64::MAX);
                if *first != next {
                    found.push(violation(
                        "I4",
                        format!("a group commit acknowledged from {first}, after {next} records"),
                    ));
                }
                log.extend(records.iter().cloned());
            }
            Entry::Caught { offset, transcript } => {
                caught = caught.saturating_add(1);
                quiescent(&log, "at quiescence", &mut found);
                caught_up(&log, *offset, transcript, &mut found);
            }
            Entry::Boot { files, .. } => {
                known = false;
                disk.boot(at, files);
            }
            Entry::Wrote { effect } => disk.wrote(at, *effect, &mut found),
            Entry::Mutation { call, effect, op } => {
                licensed_now(
                    &log,
                    known,
                    call.as_deref(),
                    *effect,
                    op,
                    stamped,
                    &mut found,
                );
                if let Op::Write { path, text } = op {
                    disk.writes.push(Write {
                        call: call.as_deref(),
                        effect: *effect,
                        path,
                        text,
                        start: at,
                        done: None,
                        end: None,
                    });
                }
                mutations.push((call.clone(), op.clone()));
            }
            Entry::Read { from, next, chunks } => reads.push(Read {
                from: *from,
                next: *next,
                chunks,
            }),
            Entry::Fault(_) | Entry::User(_) | Entry::Shell(_) => {}
        }
    }
    if caught == 0 {
        found.push(violation(
            "I5",
            "the TUI never reached quiescence".to_owned(),
        ));
    }
    let Some((recovered_at, last)) = last_recovered else {
        found.push(violation(
            "I4",
            "the daemon never recovered a log".to_owned(),
        ));
        return found;
    };
    if entries
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
    final_log(&last, &mutations, scenario, &mut found);
    edits_applied(&last, &disk, &mut found);
    edits_durable(&disk, &mut found);
    reads_exact(&last, &reads, &mut found);
    found
}

/// One `edit`'s write, as the ledger tells it.
#[derive(Debug, Clone, Copy)]
struct Write<'ledger> {
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

    /// The write of `effect` returned at ledger index `at`.
    fn wrote(&mut self, at: usize, effect: EffectId, found: &mut Vec<Violation>) {
        let running =
            self.writes.iter_mut().rev().find(|write| {
                write.effect == effect && write.done.is_none() && write.end.is_none()
            });
        match running {
            Some(write) => write.done = Some(at),
            None => found.push(violation(
                "harness",
                format!("the write of effect {effect} returned, but none was running"),
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

/// I7, edit by edit: each edit the final log says succeeded finished a
/// write that is an earlier version of its file with `old` replaced by
/// `new`.
fn edits_applied(log: &[Record<Parsed>], disk: &Disk<'_>, found: &mut Vec<Violation>) {
    let mut ignored = Vec::new();
    for call in calls(log, &mut ignored) {
        let Some(ToolArgs::Edit { path, old, new }) = &call.args else {
            continue;
        };
        if !matches!(call.results.as_slice(), [Exit::Ok]) {
            continue;
        }
        let path = path.as_str();
        let finished: Vec<&Write<'_>> = disk
            .writes
            .iter()
            .filter(|write| {
                write.call == Some(call.id.as_str())
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

/// Whether `tool` makes changes like `op`.
const fn makes(tool: Option<Tool>, op: &Op) -> bool {
    matches!(
        (tool, op),
        (Some(Tool::Edit), Op::Write { .. }) | (Some(Tool::Bash), Op::Exec { .. })
    )
}

/// I1 at the moment of a mutation: `log` is what this daemon life knew to
/// be synced (`known`: it has opened its log at all). Persist before acting:
/// a tool started before its `Dispatched` was synced fails here even when
/// the record lands a moment later.
/// The epoch `log` ends in, and the call each `Dispatched` record names,
/// by its seq.
fn dispatches(log: &[Record<Parsed>]) -> (u64, BTreeMap<u64, String>) {
    let mut epoch = 0u64;
    let mut names: Vec<&str> = Vec::new();
    let mut dispatched = BTreeMap::new();
    for (seq, record) in (0u64..).zip(log) {
        match record {
            Record::EpochBumped { epoch: bumped } => epoch = bumped.0,
            Record::TurnStarted { .. } => names.clear(),
            Record::StepFinished { calls, .. } => {
                names.extend(calls.iter().map(|proposal| proposal.id.as_str()));
            }
            Record::Dispatched { call } => {
                if let Some(name) = names.get(call.0) {
                    dispatched.insert(seq, (*name).to_owned());
                }
            }
            Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::Approved { .. }
            | Record::ToolResult { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. } => {}
        }
    }
    (epoch, dispatched)
}

fn licensed_now(
    log: &[Record<Parsed>],
    known: bool,
    call: Option<&str>,
    effect: EffectId,
    op: &Op,
    stamped: &Stamped,
    found: &mut Vec<Violation>,
) {
    // A mutation no call can be named for is reported over the final log.
    let Some(id) = call else { return };
    let at = stamped.at.as_millis();
    if !known {
        found.push(violation(
            "I1",
            format!(
                "call {id} changed the world ({op:?}) at {at}ms, before its daemon read its log"
            ),
        ));
        return;
    }
    // Malformed records are reported over the final log too.
    let mut ignored = Vec::new();
    let licensed = calls(log, &mut ignored).iter().any(|known| {
        known.id == id && known.approved > 0 && known.dispatched > 0 && makes(known.tool, op)
    });
    if !licensed {
        found.push(violation(
            "I1",
            format!(
                "call {id} changed the world ({op:?}) at {at}ms, before a synced group held its approval and dispatch"
            ),
        ));
        return;
    }
    let (epoch, dispatched) = dispatches(log);
    if dispatched.get(&effect.seq).map(String::as_str) != Some(id) {
        found.push(violation(
            "I1",
            format!(
                "call {id} changed the world ({op:?}) at {at}ms as effect {effect}, whose seq is not a synced dispatch of it"
            ),
        ));
    } else if effect.epoch.0 != epoch {
        found.push(violation(
            "I1",
            format!(
                "call {id} changed the world ({op:?}) at {at}ms as effect {effect}, outside the synced epoch {epoch}"
            ),
        ));
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

/// I1, I2, I3 and I8 over the log the final recovery found.
fn final_log(
    log: &[Record<Parsed>],
    mutations: &[(Option<String>, Op)],
    scenario: &Scenario,
    found: &mut Vec<Violation>,
) {
    let calls = calls(log, found);
    for call in &calls {
        if call.results.len() != 1 {
            found.push(violation(
                "I2",
                format!("call {} has {} results", call.id, call.results.len()),
            ));
        }
        if call.dispatched > 1 {
            found.push(violation(
                "I2",
                format!("call {} was dispatched {} times", call.id, call.dispatched),
            ));
        }
    }
    let mut acted: BTreeMap<&str, usize> = BTreeMap::new();
    for (call, op) in mutations {
        let Some(id) = call else {
            found.push(violation(
                "I1",
                format!("a mutation no call can be named for: {op:?}"),
            ));
            continue;
        };
        let licensed = calls
            .iter()
            .any(|known| known.id == *id && known.approved > 0 && known.dispatched > 0);
        if !licensed {
            found.push(violation(
                "I1",
                format!(
                    "call {id} changed the world ({op:?}) without a durable approval and dispatch"
                ),
            ));
        }
        let count = acted.entry(id.as_str()).or_default();
        *count = count.saturating_add(1);
    }
    for (id, count) in acted {
        // The fake model reuses ids across turns: each turn's call may act once.
        let allowed = calls
            .iter()
            .filter(|call| {
                call.id == id
                    && call.dispatched > 0
                    && call
                        .tool
                        .is_some_and(|tool| matches!(tool.rerun(), Rerun::Unsafe))
            })
            .count();
        if count > allowed {
            found.push(violation(
                "I3",
                format!("Unsafe call {id} acted on the world {count} times"),
            ));
        }
    }
    let mut messages = Vec::new();
    for record in log {
        if let Record::TurnStarted { message, .. } = record {
            messages.push(*message);
        }
    }
    let sent = messages.len();
    messages.sort_unstable();
    messages.dedup();
    if messages.len() != sent || sent != scenario.turns.len() {
        found.push(violation(
            "I8",
            format!(
                "{} messages were planned; the log started {sent} turns from {} distinct messages",
                scenario.turns.len(),
                messages.len()
            ),
        ));
    }
}
