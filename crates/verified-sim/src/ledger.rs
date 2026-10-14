//! The world ledger: what happened in a simulated world, outside every host.
//!
//! It lives in the harness, not on a host's disk, so it survives every crash.
//! Hosts and the harness append [`Entry`]s stamped with simulated time; the
//! checkers judge the world from it ([`crate::check`]) and the determinism
//! check compares two runs' ledgers line by line ([`Ledger::trace`]).

use core::fmt;
use core::time::Duration;
use std::collections::BTreeMap;
use std::sync::Arc;

use verified_domain::{ActorId, EffectId, Record};
use verified_server::payload::Parsed;
use verified_server::shim::Observation;
use verified_wire::chunk::UiChunk;

/// What a fake tool did to the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// An `edit` replaced the contents of `path` with `text`.
    Write { path: String, text: String },
    /// A command ran (fake `bash`); its side effects are this entry.
    Exec { command: String },
}

/// One thing that happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// The daemon host started; the project files as it found them on disk.
    Boot {
        boot: u32,
        files: BTreeMap<String, String>,
    },
    /// The daemon listed the actors' logs at boot, before it opened them.
    Listed { actors: Vec<ActorId> },
    /// An actor's empty log was created durably.
    Created { actor: ActorId },
    /// An actor's log was replayed at boot: exactly these records survived.
    Recovered {
        actor: ActorId,
        records: Vec<Record<Parsed>>,
    },
    /// A group commit of an actor's log was acknowledged as synced: these
    /// records, from `first`.
    Acked {
        actor: ActorId,
        first: u64,
        records: Vec<Record<Parsed>>,
    },
    /// The environment changed the world on behalf of tool call `call` of
    /// `actor` (`None`: no call could be named, itself a violation), run as
    /// effect `effect` (the epoch it was started in, and its `Dispatched`
    /// seq).
    Mutation {
        actor: ActorId,
        call: Option<String>,
        effect: EffectId,
        op: Op,
    },
    /// The write of `effect` of `actor` (an `edit`'s [`Op::Write`])
    /// returned: its text is in place and durable.
    Wrote { actor: ActorId, effect: EffectId },
    /// The harness injected a fault.
    Fault(String),
    /// The scripted user did something.
    User(String),
    /// The scripted user gave up its plan at turn `turn` (it may or may not
    /// have started): the TUI said the session takes no more.
    GaveUp { turn: usize },
    /// The TUI folded one stream read: the records `from..next` arrived as
    /// exactly these chunks.
    Read {
        from: u64,
        next: u64,
        chunks: Vec<UiChunk>,
    },
    /// The daemon's shell, seen through its sim-only hook: an input
    /// entering the core, the boot's resume, or a barrier reached. Recorded
    /// on the daemon's task as it happens.
    Shell(Observation),
    /// The TUI was quiescent: caught up, settled, no turn running. Its folded
    /// transcript then, and the offset just past what it had folded.
    Caught {
        offset: u64,
        transcript: serde_json::Value,
    },
}

/// An entry and when it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub at: Duration,
    pub entry: Entry,
}

/// The shared, bounded ledger of one simulated world.
#[derive(Clone)]
pub struct Ledger {
    #[expect(
        clippy::disallowed_types,
        reason = "the world ledger: every host of one single-threaded simulation appends, \
                  the harness reads after each step and after the run; it is dropped with the run"
    )]
    entries: Arc<std::sync::Mutex<Vec<Stamped>>>,
    capacity: usize,
}

impl fmt::Debug for Ledger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ledger")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl Ledger {
    /// A ledger that holds at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::default(),
            capacity,
        }
    }

    /// Appends `entry` at simulated time `at`.
    ///
    /// # Panics
    /// The ledger is full: the capacity is chosen up front, so overflowing
    /// it is a harness defect.
    pub fn record_at(&self, at: Duration, entry: Entry) {
        let mut entries = self
            .entries
            .lock()
            .expect("no ledger writer panics while holding the lock");
        assert!(
            entries.len() < self.capacity,
            "the ledger holds at most {} entries",
            self.capacity
        );
        entries.push(Stamped { at, entry });
    }

    /// Appends `entry` from inside a host, at the current simulated time.
    ///
    /// # Panics
    /// Outside a simulation, or the ledger is full.
    pub fn record(&self, entry: Entry) {
        let at = turmoil::sim_elapsed().expect("hosts record inside a simulation");
        self.record_at(at, entry);
    }

    /// `read` of every entry so far, in order, under the ledger's lock.
    ///
    /// # Panics
    /// A writer panicked while holding the lock.
    pub fn with<T, F: FnOnce(&[Stamped]) -> T>(&self, read: F) -> T {
        read(
            &self
                .entries
                .lock()
                .expect("no ledger writer panics while holding the lock"),
        )
    }

    /// Every entry so far, in order.
    #[must_use]
    pub fn entries(&self) -> Vec<Stamped> {
        self.with(<[Stamped]>::to_vec)
    }

    /// How many daemon boots the ledger has seen.
    ///
    /// # Panics
    /// More than 2^32 boots.
    #[must_use]
    pub fn boots(&self) -> u32 {
        let count = self.with(|entries| {
            entries
                .iter()
                .filter(|stamped| matches!(stamped.entry, Entry::Boot { .. }))
                .count()
        });
        u32::try_from(count).expect("fewer than 2^32 boots")
    }

    /// Where the ledger is now: the index its next entry will get.
    #[must_use]
    pub fn mark(&self) -> usize {
        self.with(<[Stamped]>::len)
    }

    /// Whether a boot since entry `since` listed the actors' logs and
    /// recovered every one.
    #[must_use]
    pub fn recovered_since(&self, since: usize) -> bool {
        self.with(|entries| {
            let after = entries.get(since..).unwrap_or_default();
            let Some(listed) = after
                .iter()
                .rposition(|stamped| matches!(stamped.entry, Entry::Listed { .. }))
            else {
                return false;
            };
            let (Some(Entry::Listed { actors }), Some(rest)) = (
                after.get(listed).map(|stamped| &stamped.entry),
                after.get(listed..),
            ) else {
                return false;
            };
            let opened = rest
                .iter()
                .filter(|stamped| matches!(stamped.entry, Entry::Recovered { .. }))
                .count();
            opened == actors.len()
        })
    }

    /// How many records the log of `actor` holds as far as anyone was
    /// told: its last recovery plus every acknowledged group commit since.
    #[must_use]
    pub fn logged(&self, actor: ActorId) -> u64 {
        self.with(|entries| {
            let mut logged = 0u64;
            for stamped in entries {
                match &stamped.entry {
                    Entry::Created { actor: of } if *of == actor => logged = 0,
                    Entry::Recovered { actor: of, records } if *of == actor => {
                        logged = u64::try_from(records.len()).unwrap_or(u64::MAX);
                    }
                    Entry::Acked {
                        actor: of, records, ..
                    } if *of == actor => {
                        let added = u64::try_from(records.len()).unwrap_or(u64::MAX);
                        logged = logged.saturating_add(added);
                    }
                    Entry::Listed { .. }
                    | Entry::Created { .. }
                    | Entry::Recovered { .. }
                    | Entry::Acked { .. }
                    | Entry::Boot { .. }
                    | Entry::Mutation { .. }
                    | Entry::Wrote { .. }
                    | Entry::Fault(_)
                    | Entry::User(_)
                    | Entry::GaveUp { .. }
                    | Entry::Read { .. }
                    | Entry::Shell(_)
                    | Entry::Caught { .. } => {}
                }
            }
            logged
        })
    }

    /// The first actor whose log was created: the root the TUI made.
    #[must_use]
    pub fn root(&self) -> Option<ActorId> {
        self.with(root)
    }

    /// The run's trace so far ([`trace`]).
    #[must_use]
    pub fn trace(&self) -> Vec<String> {
        self.with(trace)
    }
}

/// A run's trace: one line per entry, simulated time first. Two runs of one
/// seed must produce identical traces.
#[must_use]
pub fn trace(entries: &[Stamped]) -> Vec<String> {
    entries
        .iter()
        .map(|stamped| format!("{:>8}ms {:?}", stamped.at.as_millis(), stamped.entry))
        .collect()
}

/// The first actor whose log was created: the root the TUI made.
#[must_use]
pub fn root(entries: &[Stamped]) -> Option<ActorId> {
    entries.iter().find_map(|stamped| match &stamped.entry {
        Entry::Created { actor } => Some(*actor),
        Entry::Listed { .. }
        | Entry::Recovered { .. }
        | Entry::Acked { .. }
        | Entry::Boot { .. }
        | Entry::Mutation { .. }
        | Entry::Wrote { .. }
        | Entry::Fault(_)
        | Entry::User(_)
        | Entry::GaveUp { .. }
        | Entry::Read { .. }
        | Entry::Shell(_)
        | Entry::Caught { .. } => None,
    })
}
