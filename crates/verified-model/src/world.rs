//! The model's state: the durable log, the decision being persisted, the
//! effects in flight and orphaned, facts about the outside, and a client
//! following the stream.

use std::collections::{BTreeMap, BTreeSet};

use verified_core::Event;
use verified_domain::{Abstract, ActorId, CallId, EffectId, Record, Tool};

/// A record as the model sees it.
pub type Rec = Record<Abstract>;

/// What an effect in flight is doing.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Job {
    /// A model call, and the batches of parts it has streamed so far.
    Model {
        batches: u8,
    },
    Tool {
        call: CallId,
        tool: Tool,
    },
    Sleep,
    /// A stop on its way to a child.
    Stop {
        child: ActorId,
    },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Flight {
    pub job: Job,
    /// The side effect happened (tools only; others start executed).
    pub executed: bool,
    pub cancelled: bool,
}

/// An effect of the decision being persisted; it starts at `Sync`.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Staged {
    Start(EffectId, Job),
    Cancel(EffectId),
    Ask(CallId),
}

/// A client following the stream.
///
/// It holds the records it has seen, from offset 0, and the offset its next
/// read starts at. Its connection has a cursor on the server's side; a
/// reconnect (or the daemon crashing) drops it, and the client resumes on a
/// new connection from its own offset.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Default)]
pub struct Client {
    pub seen: Vec<Rec>,
    pub next: usize,
    /// The server's cursor for the live connection; `None` while it is down.
    pub cursor: Option<usize>,
    pub reconnects: u8,
    /// The offset of the last resume, if one started past the beginning.
    pub resumed_at: Option<usize>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct World {
    pub log: Vec<Rec>,
    pub pending: Vec<Rec>,
    pub staged: Vec<Staged>,
    pub flights: BTreeMap<EffectId, Flight>,
    pub orphans: BTreeMap<EffectId, Flight>,
    pub asks: BTreeSet<CallId>,
    /// Times each call's side effect happened, with its tool.
    pub runs: BTreeMap<CallId, (Tool, u8)>,
    /// Times a run of each call was started (a `RunTool` reached the world),
    /// with its tool.
    pub starts: BTreeMap<CallId, (Tool, u8)>,
    pub sent: u8,
    pub crashes: u8,
    pub interrupts: u8,
    pub duplicates: u8,
    /// Answers given to calls not awaiting one.
    pub stale_answers: u8,
    /// The last completion delivered, for late-duplicate redelivery.
    pub last: Option<Event<Abstract>>,
    pub client: Client,
    /// A reply that matched nothing live was refused, as it must be.
    pub stale_refused: bool,
    /// A reply that matched nothing live changed something.
    pub stale_changed: bool,
    /// A full log refused the boot's resume: the shell owes it, and decides
    /// it again once the running turn is interrupted. Only a replay sets
    /// it ([`crate::Observed::Refused`]); BFS never fills the log.
    pub refused: bool,
}

impl World {
    /// The shell is persisting a decision: until `Sync` (or a crash) it
    /// delivers nothing more to the core. The world goes on meanwhile.
    #[must_use]
    pub const fn persisting(&self) -> bool {
        !self.pending.is_empty() || !self.staged.is_empty()
    }

    /// Nothing pending, owed, in flight or awaiting the user.
    #[must_use]
    pub fn quiescent(&self) -> bool {
        !self.persisting() && !self.refused && self.flights.is_empty() && self.asks.is_empty()
    }
}

impl Default for World {
    fn default() -> Self {
        Self {
            log: Vec::new(),
            pending: Vec::new(),
            staged: Vec::new(),
            flights: BTreeMap::new(),
            orphans: BTreeMap::new(),
            asks: BTreeSet::new(),
            runs: BTreeMap::new(),
            starts: BTreeMap::new(),
            sent: 0,
            crashes: 0,
            interrupts: 0,
            duplicates: 0,
            stale_answers: 0,
            last: None,
            client: Client {
                cursor: Some(0),
                ..Client::default()
            },
            stale_refused: false,
            stale_changed: false,
            refused: false,
        }
    }
}
