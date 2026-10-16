//! The model's state: one daemon hosting actors, each with its durable log,
//! the decision it is persisting, its effects in flight and orphaned, and
//! facts about the outside; deliveries between their logs; and a client
//! following the stream of the first.

use std::collections::{BTreeMap, BTreeSet};

use verified_core::Event;
use verified_domain::{Abstract, CallId, EffectId, EnvelopeId, Exit, Record, Tool};

/// A record as the model sees it.
pub type Rec = Record<Abstract>;

/// An actor's place in the world: slot 0 is the first root.
pub type Slot = usize;

/// How far a `Keyed` delivery has got into its recipient's log.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Landing {
    /// A spawn's child is not yet created: its log comes first.
    Create,
    /// A spawn's child has a log; its birth is next.
    Born,
    /// The envelope is next.
    Queued,
    /// The recipient decided to append the envelope and is persisting it:
    /// it is acknowledged once that append is durable.
    Pending,
    /// The recipient answered at once (a repeat, a refusal, a limit): the
    /// delivery reports this.
    Answered(Exit),
}

/// What a delivery carries.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Delivery {
    /// A `send`'s message.
    Send,
    /// A `spawn`: the child's log, its birth, then its prompt.
    Spawn,
    /// A stop, from `v`.
    Stop,
}

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
    /// A summarize call: v's own, for a compaction of the history.
    Summarize,
    /// A `Keyed` delivery into another actor's log. `to` is the
    /// recipient, when the world knows it (a trace names it only if the
    /// envelope ever landed); `call` is the call it carries out (none for
    /// a stop).
    Deliver {
        to: Option<Slot>,
        what: Delivery,
        call: Option<CallId>,
        envelope: EnvelopeId,
        landing: Landing,
    },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Flight {
    pub job: Job,
    /// The side effect happened (tools only; others start executed).
    pub executed: bool,
    pub cancelled: bool,
    /// The world made it silent: it never reports (P1). Only its deadline
    /// resolves it.
    pub hung: bool,
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

/// One actor: its log and its shell's view of the world.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Default)]
pub struct Actor {
    /// Its log exists (a root from boot; a child once its spawn created
    /// it).
    pub created: bool,
    pub log: Vec<Rec>,
    pub pending: Vec<Rec>,
    pub staged: Vec<Staged>,
    pub flights: BTreeMap<EffectId, Flight>,
    pub orphans: BTreeMap<EffectId, Flight>,
    pub asks: BTreeSet<CallId>,
    /// Calls the user approved by command, whether or not one awaited it.
    pub approved: BTreeSet<CallId>,
    /// Times each call's side effect happened, with its tool.
    pub runs: BTreeMap<CallId, (Tool, u8)>,
    /// Times a run of each call was started (a `RunTool` reached the world),
    /// with its tool.
    pub starts: BTreeMap<CallId, (Tool, u8)>,
    /// The last completion delivered, for late-duplicate redelivery.
    pub last: Option<Event<Abstract>>,
    /// A full log refused the boot's resume: the shell fails stop until it
    /// boots again. Only a replay sets it ([`crate::Observed::Refused`]);
    /// BFS never fills the log.
    pub refused: bool,
}

impl Actor {
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

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct World {
    /// Every actor the world may hold, by slot.
    pub actors: Vec<Actor>,
    pub sent: u8,
    pub steers: u8,
    pub stops: u8,
    pub hangs: u8,
    pub crashes: u8,
    pub interrupts: u8,
    pub duplicates: u8,
    /// The user's `/compact`s.
    pub compacts: u8,
    /// Answers given to calls not awaiting one.
    pub stale_answers: u8,
    pub client: Client,
    /// A reply that matched nothing live was refused, as it must be.
    pub stale_refused: bool,
    /// A reply that matched nothing live changed something.
    pub stale_changed: bool,
    /// The calls each envelope a recipient appended carries out: its
    /// sender's slot and call, by the recipient's slot and the envelope's
    /// id. A delivery takes effect when its append is durable (C2).
    pub carried_out: BTreeMap<(Slot, EnvelopeId), (Slot, CallId)>,
    /// Deliveries acknowledged to their sender (`ToolDone` `Ok` decided):
    /// the recipient's slot and the envelope's id. An acknowledgement
    /// means the append is durable (C2, T1).
    pub acked: BTreeSet<(Slot, EnvelopeId)>,
}

impl World {
    /// A world of `slots` actors, none of them created.
    #[must_use]
    pub fn new(slots: usize) -> Self {
        Self {
            actors: vec![Actor::default(); slots],
            sent: 0,
            steers: 0,
            stops: 0,
            hangs: 0,
            crashes: 0,
            interrupts: 0,
            duplicates: 0,
            compacts: 0,
            stale_answers: 0,
            client: Client {
                cursor: Some(0),
                ..Client::default()
            },
            stale_refused: false,
            stale_changed: false,
            carried_out: BTreeMap::new(),
            acked: BTreeSet::new(),
        }
    }

    /// The first root, which the client follows.
    ///
    /// # Panics
    /// Never: a world holds at least one actor.
    #[must_use]
    pub fn root(&self) -> &Actor {
        self.actors.first().expect("a world holds its root")
    }

    /// The actor at `slot`.
    #[must_use]
    pub fn actor(&self, slot: Slot) -> Option<&Actor> {
        self.actors.get(slot)
    }

    /// Whether any actor's shell is persisting a decision.
    #[must_use]
    pub fn persisting(&self) -> bool {
        self.actors.iter().any(Actor::persisting)
    }

    /// Every actor quiescent: nothing pending, owed, in flight (no run, no
    /// task's run, no delivery) or awaiting the user.
    #[must_use]
    pub fn quiescent(&self) -> bool {
        self.actors.iter().all(Actor::quiescent)
    }
}
