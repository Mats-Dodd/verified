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

use verified_domain::{EffectId, Record};
use verified_server::payload::Parsed;
use verified_wire::chunk::UiChunk;

/// What a fake tool did to the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A file was written (an `edit`'s staged copy).
    Write { path: String },
    /// A file was renamed over another (an `edit` landing).
    Rename { from: String, to: String },
    /// A command ran (fake `bash`); its side effects are this entry.
    Exec { command: String },
    /// A staged copy that was never vouched for was removed (an `edit`
    /// whose staging failed).
    Remove { path: String },
}

/// One thing that happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// The daemon host started; the project files as it found them on disk.
    Boot {
        boot: u32,
        files: BTreeMap<String, String>,
    },
    /// A session's empty log was created durably.
    Created,
    /// A session's log was replayed at boot: exactly these records survived.
    Recovered { records: Vec<Record<Parsed>> },
    /// A group commit was acknowledged as synced: these records, from `first`.
    Acked {
        first: u64,
        records: Vec<Record<Parsed>>,
    },
    /// The environment changed the world on behalf of tool call `call`
    /// (`None`: no call could be named, itself a violation), run as effect
    /// `effect` (the epoch it was started in, and its `Dispatched` seq).
    Mutation {
        call: Option<String>,
        effect: EffectId,
        op: Op,
    },
    /// The harness injected a fault.
    Fault(String),
    /// The scripted user did something.
    User(String),
    /// The TUI folded one stream read: the records `from..next` arrived as
    /// exactly these chunks.
    Read {
        from: u64,
        next: u64,
        chunks: Vec<UiChunk>,
    },
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

    /// Every entry so far, in order.
    ///
    /// # Panics
    /// A writer panicked while holding the lock.
    #[must_use]
    pub fn entries(&self) -> Vec<Stamped> {
        self.entries
            .lock()
            .expect("no ledger writer panics while holding the lock")
            .clone()
    }

    /// How many daemon boots the ledger has seen.
    ///
    /// # Panics
    /// A writer panicked while holding the lock.
    #[must_use]
    pub fn boots(&self) -> u32 {
        let entries = self.entries();
        let count = entries
            .iter()
            .filter(|stamped| matches!(stamped.entry, Entry::Boot { .. }))
            .count();
        u32::try_from(count).expect("fewer than 2^32 boots")
    }

    /// How many log recoveries the ledger has seen.
    #[must_use]
    pub fn recoveries(&self) -> usize {
        self.entries()
            .iter()
            .filter(|stamped| matches!(stamped.entry, Entry::Recovered { .. }))
            .count()
    }

    /// How many records the daemon's log holds as far as anyone was told:
    /// the last recovery plus every acknowledged group commit since.
    #[must_use]
    pub fn logged(&self) -> u64 {
        let mut logged = 0u64;
        for stamped in self.entries() {
            match &stamped.entry {
                Entry::Created => logged = 0,
                Entry::Recovered { records } => {
                    logged = u64::try_from(records.len()).unwrap_or(u64::MAX);
                }
                Entry::Acked { records, .. } => {
                    let added = u64::try_from(records.len()).unwrap_or(u64::MAX);
                    logged = logged.saturating_add(added);
                }
                Entry::Boot { .. }
                | Entry::Mutation { .. }
                | Entry::Fault(_)
                | Entry::User(_)
                | Entry::Read { .. }
                | Entry::Caught { .. } => {}
            }
        }
        logged
    }

    /// The run's trace: one line per entry, simulated time first. Two runs of
    /// one seed must produce identical traces.
    #[must_use]
    pub fn trace(&self) -> Vec<String> {
        self.entries()
            .iter()
            .map(|stamped| format!("{:>8}ms {:?}", stamped.at.as_millis(), stamped.entry))
            .collect()
    }
}
