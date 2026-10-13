//! World-level checkers: judge what happened, not what the core claims.
//!
//! Every check reads the [`Ledger`](crate::ledger::Ledger)'s entries: what
//! the environment did to the world, what the log acknowledged as synced,
//! what each recovery found on disk, and what the TUI showed when it was
//! quiescent.
//!
//! What a tool may do to the world and when (approval and dispatch durable
//! first, an `Unsafe` call at most once), and what a crash keeps (every
//! acknowledged record, then a prefix of the pending group), is the model's
//! replay to judge ([`crate::model`]), from the same ledger.
//!
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
//! - I8 the log started one turn for each message the user sent, but the
//!   one it gave up on when the session took no more, which started at most
//!   one.

use core::fmt;
use std::collections::BTreeMap;

use verified_domain::{EffectId, Exit, Record, Request};
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
    args: Option<ToolArgs>,
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
                    let args = match &proposal.request {
                        Request::Run { args } => Some(args.clone()),
                        Request::Invalid { .. } => None,
                    };
                    calls.push(Call {
                        turn,
                        id: proposal.id.as_str().to_owned(),
                        args,
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
            Record::Dispatched { .. } => call.effects.push(seq),
            Record::ToolResult { exit, .. } => call.results.push(*exit),
            Record::Approved { .. }
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
    let mut reads: Vec<Read<'_>> = Vec::new();
    let mut disk = Disk::default();
    let mut caught = 0usize;
    let mut gave_up = None;
    for (at, stamped) in entries.iter().enumerate() {
        match &stamped.entry {
            Entry::Created => log.clear(),
            Entry::Recovered { records } => {
                log.clone_from(records);
                last_recovered = Some((at, records.clone()));
            }
            Entry::Acked { records, .. } => log.extend(records.iter().cloned()),
            Entry::Caught { offset, transcript } => {
                caught = caught.saturating_add(1);
                quiescent(&log, "at quiescence", &mut found);
                caught_up(&log, *offset, transcript, &mut found);
            }
            Entry::Boot { files, .. } => disk.boot(at, files),
            Entry::Wrote { effect } => disk.wrote(at, *effect, &mut found),
            Entry::Mutation { call, effect, op } => {
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
            }
            Entry::Read { from, next, chunks } => reads.push(Read {
                from: *from,
                next: *next,
                chunks,
            }),
            Entry::GaveUp { turn } => gave_up = Some(*turn),
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
            "harness",
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
    turns_started(&last, (scenario.turns.len(), gave_up), &mut found);
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

/// I8 over the log the final recovery found, of a plan of `planned`
/// messages the user may have given up at one.
fn turns_started(
    log: &[Record<Parsed>],
    (planned, gave_up): (usize, Option<usize>),
    found: &mut Vec<Violation>,
) {
    let started = log
        .iter()
        .filter(|record| matches!(record, Record::TurnStarted { .. }))
        .count();
    let expected = match gave_up {
        None => started == planned,
        Some(turn) => started == turn || started == turn.saturating_add(1),
    };
    if !expected {
        found.push(violation(
            "I8",
            format!(
                "{planned} messages were planned (given up at {gave_up:?}); the log started \
                 {started} turns"
            ),
        ));
    }
}
