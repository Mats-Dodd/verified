//! The model: the real core of every actor inside an abstract world, its
//! actions, and the properties every reachable state must keep.

use std::collections::{BTreeMap, BTreeSet};

use stateright::{Model, Property};
use verified_core::{
    Core, Decision, Event, Loaded, ModelExit, Outcome, Rejection, Summary, Verdict,
};
use verified_domain::{
    Abstract, ActorId, ActorName, Address, Body, Born, CallId, Class, EffectId, Envelope,
    EnvelopeId, Epoch, Exit, Grant, Limits, Measured, MessageId, ModelFault, Proposal, Record,
    Request, Rules, Shape, StepId, TaskId, Tool, TurnOutcome, Unit, UnixMillis, Unstarted,
};

use crate::alpha::{self, Work};
use crate::audit::{Audit, Breach, audit};
use crate::world::{Actor, Delivery, Flight, Job, Landing, Rec, Slot, Staged, World};

/// One step of the world.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    /// An event enters the core of the actor at the slot: a user's command,
    /// or a reply of an effect in flight. It carries the event's values
    /// (ids, sizes, a model's proposals), so a trace's events are actions
    /// as they are; which values are offered is the [`Domain`]'s.
    Deliver(Slot, Event<Abstract>),
    /// A tool's side effect happens: the shell's `ToolRan` point, where its
    /// run has returned.
    Execute(Slot, EffectId),
    OrphanExecute(Slot, EffectId),
    OrphanComplete(Slot, EffectId),
    Duplicate(Slot),
    /// The decision the actor's shell is persisting becomes durable and
    /// its effects start.
    Sync(Slot),
    /// The daemon crashes, keeping this many of each actor's pending
    /// records: one process, so every actor crashes at once.
    Crash(Vec<usize>),
    /// A delivery in flight takes its next step into its recipient's log:
    /// a spawn's child is created, then born; an envelope is appended.
    Land(Slot, EffectId),
    /// The world makes a run silent: it never reports.
    Hang(Slot, EffectId),
    /// The client reads the next committed record on its connection.
    Read,
    /// The client opens a new connection, resuming from its offset.
    Reconnect,
}

/// The kind of an [`Action`], without its values: a delivery by its
/// event's.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// The actor is born (at boot, never an action of BFS).
    Born,
    /// The rules file changed (never an action of BFS).
    Rules,
    /// An envelope of text: the user's message, or an agent's.
    Send,
    /// A stop envelope.
    Stop,
    Answer,
    Interrupt,
    Execute,
    Parts,
    Finish,
    Fail,
    ToolDone,
    Woke,
    /// The shell's deadline for a run passed.
    Expire,
    /// The user's `/compact`.
    Compact,
    /// A summarize call answered.
    Summarize,
    OrphanExecute,
    OrphanComplete,
    Duplicate,
    Sync,
    Crash,
    Land,
    Hang,
    Read,
    Reconnect,
}

impl Action {
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Deliver(_, Event::Born { .. }) => Kind::Born,
            Self::Deliver(_, Event::Rules { .. }) => Kind::Rules,
            Self::Deliver(
                _,
                Event::Deliver {
                    envelope:
                        Envelope {
                            body: Body::Stop, ..
                        },
                },
            ) => Kind::Stop,
            Self::Deliver(_, Event::Deliver { .. }) => Kind::Send,
            Self::Deliver(_, Event::Approve { .. }) => Kind::Answer,
            Self::Deliver(_, Event::Interrupt { .. }) => Kind::Interrupt,
            Self::Deliver(_, Event::ModelParts { .. }) => Kind::Parts,
            Self::Deliver(
                _,
                Event::ModelDone {
                    result: ModelExit::Finished(_),
                    ..
                },
            ) => Kind::Finish,
            Self::Deliver(
                _,
                Event::ModelDone {
                    result: ModelExit::Failed(_),
                    ..
                },
            ) => Kind::Fail,
            Self::Deliver(_, Event::ToolDone { .. }) => Kind::ToolDone,
            Self::Deliver(_, Event::Woke { .. }) => Kind::Woke,
            Self::Deliver(_, Event::Expired { .. }) => Kind::Expire,
            Self::Deliver(_, Event::Compact) => Kind::Compact,
            Self::Deliver(_, Event::Summarized { .. }) => Kind::Summarize,
            Self::Execute(..) => Kind::Execute,
            Self::OrphanExecute(..) => Kind::OrphanExecute,
            Self::OrphanComplete(..) => Kind::OrphanComplete,
            Self::Duplicate(_) => Kind::Duplicate,
            Self::Sync(_) => Kind::Sync,
            Self::Crash(_) => Kind::Crash,
            Self::Land(..) => Kind::Land,
            Self::Hang(..) => Kind::Hang,
            Self::Read => Kind::Read,
            Self::Reconnect => Kind::Reconnect,
        }
    }

    /// The event this action delivers, if it delivers one.
    #[must_use]
    pub const fn event(&self) -> Option<&Event<Abstract>> {
        match self {
            Self::Deliver(_, event) => Some(event),
            Self::Execute(..)
            | Self::OrphanExecute(..)
            | Self::OrphanComplete(..)
            | Self::Duplicate(_)
            | Self::Sync(_)
            | Self::Crash(_)
            | Self::Land(..)
            | Self::Hang(..)
            | Self::Read
            | Self::Reconnect => None,
        }
    }

    /// The slot of the actor this action steps, if it steps one.
    #[must_use]
    pub const fn slot(&self) -> Option<Slot> {
        match self {
            Self::Deliver(slot, _)
            | Self::Execute(slot, _)
            | Self::OrphanExecute(slot, _)
            | Self::OrphanComplete(slot, _)
            | Self::Duplicate(slot)
            | Self::Sync(slot)
            | Self::Land(slot, _)
            | Self::Hang(slot, _) => Some(*slot),
            Self::Crash(_) | Self::Read | Self::Reconnect => None,
        }
    }
}

/// The values actions may carry.
#[derive(Clone, Debug)]
pub enum Domain {
    /// A small synthetic set, for BFS.
    Bounded(Bounded),
    /// The events a trace delivers (each once), each with the slot of the
    /// actor it entered, to replay it.
    Trace(Vec<(Slot, Event<Abstract>)>),
}

/// The BFS's values: one-byte payloads, ids made from small indices, and
/// bounds on the world's faults.
#[derive(Clone, Debug)]
pub struct Bounded {
    /// Distinct messages the user may send the first root; one extra send
    /// repeats the first.
    pub messages: usize,
    /// Each slot's possible model responses (a slot past the end has the
    /// last slot's), each the calls it proposes. Call ids are made from
    /// the model call's id, unique per call as the provider's are. A
    /// `spawn` names the next slot's actor, a `send` goes to the next slot
    /// (round the ring), and a `cancel` names the actor's oldest running
    /// task.
    pub scripts: Vec<Vec<Vec<Shape>>>,
    /// Batches of parts a model call may stream before it ends.
    pub batches: u8,
    /// Whether a model call may fail (overloaded, other, cancelled).
    pub faults: bool,
    /// Answers the user may give to a proposed call not awaiting one:
    /// repeated, late, racing another, or after a crash dropped the ask.
    pub stale_answers: u8,
    pub max_crashes: u8,
    pub max_interrupts: u8,
    pub max_duplicates: u8,
    /// Whether a client follows the stream (and may reconnect this often).
    pub client: Option<u8>,
    /// Actors the world may hold.
    pub slots: usize,
    /// The first of them, born at boot (the rest are born by `spawn`).
    pub roots: usize,
    /// The registry's cap on actors (P1).
    pub actors: usize,
    /// Messages the user may send the first root while a turn runs.
    pub steers: u8,
    /// Stops the user may send the first root.
    pub stops: u8,
    pub max_hangs: u8,
    /// Whether the shell's deadlines fire (P1).
    pub expiries: bool,
    /// The rules file every actor's shell loads (`None`: there is none, so
    /// the default rules); the harness takes it.
    pub rules: Option<Rules>,
    /// The size of each of the user's messages.
    pub message_bytes: u64,
    /// The sizes of the summaries a summarize call may write (one that
    /// makes room, one that does not).
    pub summaries: Vec<u64>,
    /// `/compact`s the user may send the first root.
    pub compacts: u8,
}

/// A deliberately broken shell, to show the properties can fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// After a crash, rerun dispatched `Unsafe` calls as if they were `Safe`.
    RerunUnsafe,
    /// After a crash, resolve dispatched `Unsafe` calls `NotStarted`, as
    /// if the shell knew they never ran.
    UnsafeResolvedNotStarted,
    /// The stream serves records that are decided but not yet durable.
    ServePending,
    /// A reconnecting client resumes one record past its offset.
    SkipOnReconnect,
    /// The shell approves what it should ask about, as if by the first rule
    /// in force, whether or not it fits the call.
    RuleApprovesAll,
    /// The shell approves what it should ask about in the user's name,
    /// without the user's command.
    UserApprovesAll,
    /// After a crash, drop the `Unknown` result of a dispatched `Unsafe`
    /// call, and all the resume decided after it, so the call stays
    /// dispatched.
    LeaveDispatched,
    /// The outbox delivers before the record that licenses the delivery is
    /// durable (C1).
    DeliverBeforeDurable,
    /// After a crash, a `Keyed` call is sent again under a fresh key, so
    /// its recipient cannot tell it from a new one (C2).
    FreshKeyOnResume,
    /// The sender is told a delivery is done once its recipient decided to
    /// append it, before the append is durable (C2).
    AckBeforeDurable,
    /// After a crash, the resume ends again a task that already ended
    /// (T3).
    EndTaskTwice,
    /// After a crash, drop the `Unknown` end of a running `Unsafe` task,
    /// and all the resume decided after it, so the task never ends (T3).
    LeaveTaskRunning,
    /// The shell starts a step without the mail at the head of the
    /// mailbox: it never reaches the model (T4).
    SkipCarry,
    /// The shell starts a step carrying again an envelope a step carried
    /// before (T4).
    CarryTwice,
    /// A user's message is spliced into a step whose calls have not all
    /// resolved (T4).
    SpliceSteering,
    /// A `send` blocks its turn until the recipient's turn that read it
    /// ends: an `ask` (P1).
    BlockingSend,
    /// The shell arms no deadline: a silent run is waited on for ever (P1).
    NoDeadline,
    /// The registry ignores its cap on actors (P1).
    NoActorLimit,
    /// A stop has a deadline like any delivery, and v gives up on one that
    /// misses it (C2): resolved `Unknown`, it is never sent again.
    StopExpires,
    /// A stop to a child lands while its parent's spawn of that child is
    /// in flight (C2): the child is unborn, so the stop finds no actor,
    /// and the spawn then births it.
    StopBeforeBirth,
}

/// The core under `limits` in an abstract world.
///
/// Its state is [`World`], and each actor's core is always `recover(log)`:
/// it has no other state. Each root boots as the daemon boots it, by
/// persisting `resume()` of the empty log, which records who the actor is
/// (`born`); a child is created and born by its parent's `spawn`.
#[derive(Clone, Debug)]
pub struct Harness {
    pub limits: Limits,
    pub domain: Domain,
    pub bug: Option<Bug>,
    /// The first root: who the shell says it is at every boot.
    pub born: Born,
    /// The actors at slots 1 and on.
    pub others: Vec<ActorId>,
    /// The rules file every actor's shell loads (`None`: there is none, so
    /// the default rules).
    pub rules: Option<Rules>,
    /// The size of that file's text, as its `RulesLoaded` measures it.
    pub file_bytes: u64,
}

/// The time every bounded id is minted at.
fn epoch_time(offset: u64) -> UnixMillis {
    UnixMillis::new(1_700_000_000_000u64.saturating_add(offset)).expect("a 48-bit time")
}

/// The actor at `slot` of a bounded world.
fn actor_id(slot: Slot) -> ActorId {
    let byte = u8::try_from(slot)
        .ok()
        .and_then(|slot| slot.checked_add(3))
        .unwrap_or(u8::MAX);
    ActorId::from_parts(epoch_time(0), &[byte; 10])
}

/// Every tool: an actor in a bounded world is offered all of them.
const TOOLS: [Tool; 8] = [
    Tool::Read,
    Tool::Edit,
    Tool::Bash,
    Tool::Spawn,
    Tool::Send,
    Tool::Wake,
    Tool::Tasks,
    Tool::Cancel,
];

/// The actor a harness models unless a trace says otherwise.
///
/// # Panics
/// Never: the name and the time are constants.
#[must_use]
pub fn born() -> Born {
    Born::root(
        actor_id(0),
        ActorName::parse("main").expect("a name"),
        vec![Tool::Read, Tool::Edit, Tool::Bash],
    )
}

/// The id of message `index` of a bounded domain.
fn message_id(index: usize) -> MessageId {
    let number = u64::try_from(index).expect("few messages");
    MessageId::from_parts(epoch_time(number), &[1; 10])
}

/// The id of the user's stop `index` of a bounded domain.
fn stop_id(index: u8) -> MessageId {
    MessageId::from_parts(epoch_time(u64::from(index)), &[2; 10])
}

/// The user's message `message` to `actor`.
#[must_use]
pub const fn message(actor: ActorId, message: MessageId) -> Event<Abstract> {
    sized(actor, message, 1)
}

/// The user's message `message` to `actor`, of `bytes`.
const fn sized(actor: ActorId, message: MessageId, bytes: u64) -> Event<Abstract> {
    Event::Deliver {
        envelope: Envelope {
            id: EnvelopeId::User(message),
            from: Address::User,
            to: actor,
            reply_to: None,
            body: Body::Text(size(bytes)),
        },
    }
}

/// The user's stop `stop` to `actor`.
#[must_use]
pub const fn stop(actor: ActorId, stop: MessageId) -> Event<Abstract> {
    Event::Deliver {
        envelope: Envelope {
            id: EnvelopeId::User(stop),
            from: Address::User,
            to: actor,
            reply_to: None,
            body: Body::Stop,
        },
    }
}

/// A plain call of `tool`, without a subject: a foreground run, a spawn
/// and a cancel to be named by the harness.
#[must_use]
pub fn plain(tool: Tool) -> Shape {
    Shape::plain(tool, actor_id(0), TaskId(0))
}

/// A command run in the background: a task.
#[must_use]
pub const fn background() -> Shape {
    Shape::Bash {
        subject: Vec::new(),
        background: true,
    }
}

const fn size(bytes: u64) -> Measured<Unit> {
    Measured { body: Unit, bytes }
}

/// Whether `flight`, effect `id`, may reply `event`: a reply of its own
/// job's kind, a cancelled one only once it was asked to stop, and a tool's
/// other reports only once it ran. A cancelled run reports `NotStarted`
/// only if it never acted (T3); a run that acted and was stopped (a cancel,
/// or its own timeout) reports `Unknown`. A delivery reports what its
/// recipient answered, and `Ok` for an append only once it is durable
/// (`durable`).
fn admits(id: EffectId, flight: &Flight, durable: bool, event: &Event<Abstract>) -> bool {
    if flight.hung {
        return false;
    }
    let (of, admitted) = match event {
        Event::ModelParts { id, .. } => (id, matches!(flight.job, Job::Model { .. })),
        Event::ModelDone { id, result } => (
            id,
            matches!(flight.job, Job::Model { .. })
                && (flight.cancelled
                    || !matches!(result, ModelExit::Failed(ModelFault::Cancelled))),
        ),
        Event::ToolDone { id, exit, .. } => (
            id,
            match &flight.job {
                Job::Deliver { landing, .. } => match landing {
                    Landing::Answered(answer) => answer == exit,
                    Landing::Pending => *exit == Exit::Ok && durable,
                    Landing::Create | Landing::Born | Landing::Queued => false,
                },
                Job::Tool { .. } => match exit {
                    Exit::NotStarted(_) => flight.cancelled && !flight.executed,
                    // `Unknown`: a run stopped mid-way (a cancel, or its own
                    // timeout).
                    Exit::Ok | Exit::Failed | Exit::Unknown => flight.executed,
                },
                Job::Model { .. } | Job::Sleep | Job::Summarize => false,
            },
        ),
        Event::Woke { id } => (id, matches!(flight.job, Job::Sleep)),
        Event::Summarized { id, summary } => (
            id,
            matches!(flight.job, Job::Summarize)
                && (flight.cancelled || !matches!(summary, Summary::Failed(ModelFault::Cancelled))),
        ),
        Event::Born { .. }
        | Event::Rules { .. }
        | Event::Deliver { .. }
        | Event::Approve { .. }
        | Event::Interrupt { .. }
        | Event::Expired { .. }
        | Event::Compact => return false,
    };
    *of == id && admitted
}

impl Bounded {
    /// The model responses offered to the actor at `slot`.
    fn scripts(&self, slot: Slot) -> &[Vec<Shape>] {
        self.scripts
            .get(slot)
            .or_else(|| self.scripts.last())
            .map_or(&[], Vec::as_slice)
    }
}

impl Harness {
    /// A harness for BFS under `limits` over `bounded`.
    #[must_use]
    pub fn bounded(limits: Limits, bounded: Bounded) -> Self {
        let others = (1..bounded.slots.max(1)).map(actor_id).collect();
        Self {
            limits,
            rules: bounded.rules.clone(),
            file_bytes: 1,
            domain: Domain::Bounded(bounded),
            bug: None,
            born: born(),
            others,
        }
    }

    /// A harness for replaying the trace of one actor, run under `limits`:
    /// its deliveries carry the values `events` do.
    #[must_use]
    pub fn trace<E: IntoIterator<Item = Event<Abstract>>>(limits: Limits, events: E) -> Self {
        Self::traced(limits, events.into_iter().map(|event| (0, event)))
    }

    /// A harness for replaying the traces of several actors, run under
    /// `limits`: its deliveries carry the values `events` do, each into
    /// the actor at its slot.
    #[must_use]
    pub fn traced<E: IntoIterator<Item = (Slot, Event<Abstract>)>>(
        limits: Limits,
        events: E,
    ) -> Self {
        let mut distinct: Vec<(Slot, Event<Abstract>)> = Vec::new();
        for event in events {
            if !distinct.contains(&event) {
                distinct.push(event);
            }
        }
        Self {
            limits,
            domain: Domain::Trace(distinct),
            bug: None,
            born: born(),
            others: Vec::new(),
            rules: None,
            file_bytes: 1,
        }
    }

    /// This harness, for the first root `born` (the one a trace's daemon
    /// booted).
    #[must_use]
    pub fn with_born(self, born: Born) -> Self {
        Self { born, ..self }
    }

    /// This harness, with these actors at slots 1 and on.
    #[must_use]
    pub fn with_others(self, others: Vec<ActorId>) -> Self {
        Self { others, ..self }
    }

    /// This harness, its shells loading `rules`.
    #[must_use]
    pub fn with_rules(self, rules: Option<Rules>) -> Self {
        Self { rules, ..self }
    }

    /// This harness, its shells loading what a trace's resume recorded:
    /// `rules` from a file of this size, or with no file the defaults.
    #[must_use]
    pub fn with_loaded(self, rules: Rules, file: Option<Measured<Unit>>) -> Self {
        match file {
            Some(file) => Self {
                rules: Some(rules),
                file_bytes: file.bytes,
                ..self
            },
            None => Self {
                rules: None,
                ..self
            },
        }
    }

    /// Actors the world may hold.
    #[must_use]
    pub const fn slots(&self) -> usize {
        self.others.len().saturating_add(1)
    }

    /// The roots, born at boot.
    const fn roots(&self) -> usize {
        match &self.domain {
            Domain::Bounded(bounded) => bounded.roots,
            Domain::Trace(_) => 1,
        }
    }

    /// The actor at `slot`.
    #[must_use]
    pub fn id(&self, slot: Slot) -> Option<ActorId> {
        match slot.checked_sub(1) {
            None => Some(self.born.actor),
            Some(other) => self.others.get(other).copied(),
        }
    }

    /// The slot of `actor`.
    #[must_use]
    pub fn slot_of(&self, actor: ActorId) -> Option<Slot> {
        if actor == self.born.actor {
            return Some(0);
        }
        self.others
            .iter()
            .position(|other| *other == actor)
            .map(|at| at.saturating_add(1))
    }

    /// Who the root at `slot` is, as its shell says at every boot.
    fn root(&self, slot: Slot) -> Option<Born> {
        if slot == 0 {
            return Some(self.born.clone());
        }
        if slot >= self.roots() {
            return None;
        }
        Some(Born::root(
            self.id(slot)?,
            ActorName::parse("peer").ok()?,
            TOOLS.to_vec(),
        ))
    }

    /// The rules every shell loads at its first boot.
    fn loaded(&self) -> Loaded<Abstract> {
        match &self.rules {
            Some(rules) => Loaded {
                rules: rules.clone(),
                file: Some(size(self.file_bytes)),
            },
            None => Loaded::defaults(),
        }
    }

    /// The rules the shell reads at a boot: once, if none are in force.
    fn load(&self, core: &Core<Abstract>) -> Option<Loaded<Abstract>> {
        core.rules().is_none().then(|| self.loaded())
    }

    /// Whether the world may crash again.
    const fn may_crash(&self, world: &World) -> bool {
        match &self.domain {
            Domain::Bounded(bounded) => world.crashes < bounded.max_crashes,
            Domain::Trace(_) => true,
        }
    }

    fn audit(&self, actor: &Actor) -> Audit {
        audit(&actor.log, &self.limits)
    }

    /// The audit of every actor's log.
    fn audits<'world>(&self, world: &'world World) -> impl Iterator<Item = Audit> + 'world {
        let limits = self.limits.clone();
        world
            .actors
            .iter()
            .map(move |actor| audit(&actor.log, &limits))
    }

    /// The core of the actor at `slot`: `recover` of its log.
    pub(crate) fn core(&self, world: &World, slot: Slot) -> Option<Core<Abstract>> {
        Core::recover(self.limits.clone(), &world.actor(slot)?.log).ok()
    }

    /// Stages `decision` as the decision the actor at `slot` persists.
    /// `resumed` is the length of its log when the decision is a resume.
    fn stage(
        &self,
        world: &mut World,
        slot: Slot,
        decision: &Decision<Abstract>,
        resumed: Option<usize>,
    ) {
        let decided = alpha::decision(decision);
        let rules = self
            .core(world, slot)
            .and_then(|core| core.rules().map(|(seq, _)| *seq));
        let jobs: Vec<Staged> = decided
            .effects
            .iter()
            .map(|work| self.job(world, slot, work, resumed))
            .collect();
        let bug = self.bug;
        let Some(actor) = world.actors.get_mut(slot) else {
            return;
        };
        actor.pending = decided.records;
        for work in &decided.effects {
            if let Work::AskApproval { call, .. } = work {
                // The shell approves what it should ask about, as if by
                // rule, or in your name.
                let by = match (bug, rules) {
                    (Some(Bug::RuleApprovesAll), Some(rules)) => {
                        Some(Grant::Rule { rule: 0, rules })
                    }
                    (Some(Bug::UserApprovesAll), _) => Some(Grant::User),
                    _ => None,
                };
                if let Some(by) = by {
                    actor.pending.push(Record::Approved { call: *call, by });
                }
            }
        }
        if bug == Some(Bug::SkipCarry) {
            for record in &mut actor.pending {
                if let Record::StepStarted { carries, .. } = record {
                    carries.clear();
                }
            }
        }
        if bug == Some(Bug::CarryTwice) {
            let carried = actor.log.iter().find_map(|record| {
                if let Record::StepStarted { carries, .. } = record {
                    carries.first().copied()
                } else {
                    None
                }
            });
            if let Some(again) = carried {
                for record in &mut actor.pending {
                    if let Record::StepStarted { carries, .. } = record
                        && !carries.is_empty()
                    {
                        carries.push(again);
                    }
                }
            }
        }
        actor.staged = Vec::new();
        for staged in jobs {
            match staged {
                Staged::Start(id, job)
                    if bug == Some(Bug::DeliverBeforeDurable)
                        && matches!(job, Job::Deliver { .. }) =>
                {
                    // The outbox takes it at once.
                    actor.flights.insert(id, Flight::new(job));
                }
                staged @ (Staged::Start(..) | Staged::Cancel(_) | Staged::Ask(_)) => {
                    actor.staged.push(staged);
                }
            }
        }
        if actor.pending.is_empty() {
            // Nothing to persist: the shell starts the effects at once.
            Self::sync(world, slot);
        }
    }

    /// What the world does for `work`, an effect of the actor at `slot`.
    fn job(&self, world: &World, slot: Slot, work: &Work, resumed: Option<usize>) -> Staged {
        let sender = self.id(slot).unwrap_or(self.born.actor);
        let minted = |seq: u64| EnvelopeId::Minted { actor: sender, seq };
        match work {
            Work::CallModel { id, .. } => Staged::Start(*id, Job::Model { batches: 0 }),
            Work::RunTool {
                id: call,
                tool,
                shape,
                effect,
                ..
            } => {
                // A rerun is sent under its key, the seq of its
                // `Dispatched`; the planted shell mints a fresh one.
                let from = resumed.and_then(|from| u64::try_from(from).ok());
                let key = match from {
                    Some(from) if self.bug == Some(Bug::FreshKeyOnResume) && effect.seq < from => {
                        from
                    }
                    Some(_) | None => effect.seq,
                };
                let job = match tool {
                    Tool::Send => Job::Deliver {
                        to: self.recipient(world, slot, minted(key)),
                        what: Delivery::Send,
                        call: Some(call.clone()),
                        envelope: minted(key),
                        landing: Landing::Queued,
                    },
                    Tool::Spawn => Job::Deliver {
                        to: shape.child().and_then(|child| self.slot_of(child)),
                        what: Delivery::Spawn,
                        call: Some(call.clone()),
                        envelope: minted(key),
                        landing: Landing::Create,
                    },
                    Tool::Read
                    | Tool::Edit
                    | Tool::Bash
                    | Tool::Wake
                    | Tool::Tasks
                    | Tool::Cancel => Job::Tool {
                        call: call.clone(),
                        tool: *tool,
                    },
                };
                Staged::Start(*effect, job)
            }
            Work::Sleep { id, .. } => Staged::Start(*id, Job::Sleep),
            Work::Summarize { id } => Staged::Start(*id, Job::Summarize),
            Work::Stop { id, child } => Staged::Start(
                *id,
                Job::Deliver {
                    to: self.slot_of(*child),
                    what: Delivery::Stop,
                    call: None,
                    envelope: minted(id.seq),
                    landing: Landing::Queued,
                },
            ),
            Work::CancelModel { id } | Work::CancelTool { id } => Staged::Cancel(*id),
            Work::AskApproval { id, .. } => Staged::Ask(id.clone()),
        }
    }

    /// Where a `send` of the actor at `slot` goes: the next slot round the
    /// ring, or in a trace the actor the envelope landed at.
    fn recipient(&self, world: &World, slot: Slot, envelope: EnvelopeId) -> Option<Slot> {
        match &self.domain {
            Domain::Bounded(_) => slot
                .checked_add(1)
                .and_then(|next| next.checked_rem(world.actors.len())),
            Domain::Trace(events) => events.iter().find_map(|(to, event)| match event {
                Event::Deliver { envelope: landed } if landed.id == envelope => Some(*to),
                Event::Born { .. }
                | Event::Rules { .. }
                | Event::Deliver { .. }
                | Event::Approve { .. }
                | Event::Interrupt { .. }
                | Event::ModelParts { .. }
                | Event::ModelDone { .. }
                | Event::ToolDone { .. }
                | Event::Woke { .. }
                | Event::Expired { .. }
                | Event::Compact
                | Event::Summarized { .. } => None,
            }),
        }
    }

    /// The envelope a delivery of the actor at `slot` appends at `to`.
    fn envelope(&self, slot: Slot, to: Slot, job: &Job) -> Option<Envelope<Abstract>> {
        let Job::Deliver { envelope, what, .. } = job else {
            return None;
        };
        match &self.domain {
            Domain::Bounded(_) => {
                let sender = self.id(slot)?;
                Some(Envelope {
                    id: *envelope,
                    // A stop is v's; a spawn's prompt replies to its parent.
                    from: match what {
                        Delivery::Send | Delivery::Spawn => Address::Actor(sender),
                        Delivery::Stop => Address::Runtime,
                    },
                    to: self.id(to)?,
                    reply_to: match what {
                        Delivery::Spawn => Some(Address::Actor(sender)),
                        Delivery::Send | Delivery::Stop => None,
                    },
                    body: match what {
                        Delivery::Send | Delivery::Spawn => Body::Text(size(1)),
                        Delivery::Stop => Body::Stop,
                    },
                })
            }
            Domain::Trace(events) => events.iter().find_map(|(at, event)| match event {
                Event::Deliver { envelope: landed } if *at == to && landed.id == *envelope => {
                    Some(landed.clone())
                }
                Event::Born { .. }
                | Event::Rules { .. }
                | Event::Deliver { .. }
                | Event::Approve { .. }
                | Event::Interrupt { .. }
                | Event::ModelParts { .. }
                | Event::ModelDone { .. }
                | Event::ToolDone { .. }
                | Event::Woke { .. }
                | Event::Expired { .. }
                | Event::Compact
                | Event::Summarized { .. } => None,
            }),
        }
    }

    /// Who the child at `to`, spawned by the actor at `slot`, is born as.
    fn child(&self, world: &World, slot: Slot, to: Slot) -> Option<Born> {
        match &self.domain {
            Domain::Bounded(_) => {
                let parent = self.core(world, slot)?.born()?.clone();
                Some(Born {
                    actor: self.id(to)?,
                    name: ActorName::parse("child").ok()?,
                    parent: Some(parent.actor),
                    depth: parent.depth.saturating_add(1),
                    tools: TOOLS.to_vec(),
                })
            }
            Domain::Trace(events) => events.iter().find_map(|(at, event)| match event {
                Event::Born { born } if *at == to => Some(born.clone()),
                Event::Born { .. }
                | Event::Rules { .. }
                | Event::Deliver { .. }
                | Event::Approve { .. }
                | Event::Interrupt { .. }
                | Event::ModelParts { .. }
                | Event::ModelDone { .. }
                | Event::ToolDone { .. }
                | Event::Woke { .. }
                | Event::Expired { .. }
                | Event::Compact
                | Event::Summarized { .. } => None,
            }),
        }
    }

    /// The one state the world boots into: every root persisting
    /// `resume()` of its empty log.
    pub(crate) fn boot(&self) -> World {
        let mut world = World::new(self.slots());
        for slot in 0..self.roots().min(self.slots()) {
            if let Some(actor) = world.actors.get_mut(slot) {
                actor.created = true;
            }
            let boot = Core::<Abstract>::new(self.limits.clone())
                .resume(self.root(slot), Some(self.loaded()));
            self.stage(&mut world, slot, &boot, Some(0));
        }
        world
    }

    /// The decision the actor at `slot` is persisting is durable: its
    /// records join the log and its effects start.
    fn sync(world: &mut World, slot: Slot) {
        let Some(actor) = world.actors.get_mut(slot) else {
            return;
        };
        let mut pending = std::mem::take(&mut actor.pending);
        actor.log.append(&mut pending);
        for staged in std::mem::take(&mut actor.staged) {
            match staged {
                Staged::Start(id, job) => {
                    if let Job::Tool { call, tool } = &job {
                        let entry = actor.starts.entry(call.clone()).or_insert((*tool, 0));
                        entry.1 = entry.1.saturating_add(1);
                    }
                    actor.flights.insert(id, Flight::new(job));
                }
                Staged::Cancel(id) => {
                    if let Some(flight) = actor.flights.get_mut(&id) {
                        flight.cancelled = true;
                    }
                }
                Staged::Ask(call) => {
                    actor.asks.insert(call);
                }
            }
        }
    }

    /// Delivers `event` to the core of the actor at `slot` and stages its
    /// decision; nothing, on a log that does not recover (every durable
    /// prefix recovers is a property).
    fn deliver(&self, world: &mut World, slot: Slot, event: Event<Abstract>) -> Option<Outcome> {
        let core = self.core(world, slot)?;
        let completion = matches!(
            event,
            Event::ModelDone { .. }
                | Event::ToolDone { .. }
                | Event::Woke { .. }
                | Event::Summarized { .. }
        );
        if completion && let Some(actor) = world.actors.get_mut(slot) {
            actor.last = Some(event.clone());
        }
        let decision = core.decide(event);
        self.stage(world, slot, &decision, None);
        Some(decision.outcome)
    }

    /// Delivers a reply that matches nothing live; it must change nothing.
    fn deliver_stale(&self, world: &mut World, slot: Slot, event: Event<Abstract>) {
        let Some(core) = self.core(world, slot) else {
            return;
        };
        let decision = core.decide(event);
        if decision.outcome == Outcome::Stale
            && decision.records.is_empty()
            && decision.effects.is_empty()
        {
            world.stale_refused = true;
        } else {
            world.stale_changed = true;
        }
    }

    /// A tool's side effect happens in the world.
    fn run(actor: &mut Actor, job: &Job) {
        if let Job::Tool { call, tool } = job {
            let entry = actor.runs.entry(call.clone()).or_insert((*tool, 0));
            entry.1 = entry.1.saturating_add(1);
        }
    }

    const fn reply(id: EffectId, flight: &Flight) -> Event<Abstract> {
        match &flight.job {
            Job::Model { .. } => Event::ModelDone {
                id,
                result: ModelExit::Finished(Vec::new()),
            },
            Job::Tool { .. } | Job::Deliver { .. } => Event::ToolDone {
                id,
                exit: Exit::Ok,
                output: size(1),
            },
            Job::Sleep => Event::Woke { id },
            Job::Summarize => Event::Summarized {
                id,
                summary: Summary::Failed(ModelFault::Other),
            },
        }
    }

    /// What the stream serves a reader: the root's durable log (and, under
    /// the `ServePending` bug, the decision still being persisted).
    fn served(&self, world: &World) -> Vec<Rec> {
        let root = world.root();
        let mut served = root.log.clone();
        if self.bug == Some(Bug::ServePending) {
            served.extend(root.pending.iter().cloned());
        }
        served
    }

    fn client_actions(&self, state: &World, actions: &mut Vec<Action>) {
        // A dropped connection is always reopened; a live one may also drop
        // (in BFS, up to the client's bound).
        let may_drop = match &self.domain {
            Domain::Bounded(Bounded { client: None, .. }) => return,
            Domain::Bounded(Bounded {
                client: Some(reconnects),
                ..
            }) => state.client.reconnects < *reconnects,
            Domain::Trace(_) => true,
        };
        if let Some(cursor) = state.client.cursor
            && self.served(state).len() > cursor
        {
            actions.push(Action::Read);
        }
        if state.client.cursor.is_none() || may_drop {
            actions.push(Action::Reconnect);
        }
    }

    /// Side effects of runs already started: tool tasks run beside the
    /// shell, whatever it is doing, and (in BFS) a crash does not stop
    /// them.
    fn executions(&self, state: &World, slot: Slot, actions: &mut Vec<Action>) {
        let Some(actor) = state.actor(slot) else {
            return;
        };
        for (id, flight) in &actor.flights {
            if matches!(flight.job, Job::Tool { .. }) && !flight.executed {
                actions.push(Action::Execute(slot, *id));
            }
        }
        let hangs = match &self.domain {
            Domain::Bounded(bounded) => {
                for (id, flight) in &actor.orphans {
                    if matches!(flight.job, Job::Tool { .. }) && !flight.executed {
                        actions.push(Action::OrphanExecute(slot, *id));
                    }
                }
                state.hangs < bounded.max_hangs
            }
            // A trace's run hangs where the run was seen to.
            Domain::Trace(_) => true,
        };
        if hangs {
            for (id, flight) in &actor.flights {
                if matches!(flight.job, Job::Tool { .. }) && !flight.hung {
                    actions.push(Action::Hang(slot, *id));
                }
            }
        }
    }

    /// The next steps of the deliveries of the actor at `slot`: each lands
    /// when its recipient's shell takes input. A stop to a child waits for
    /// its parent's spawn of that child in flight, so it never finds the
    /// child unborn while the spawn may still birth it.
    fn landings(&self, state: &World, slot: Slot, actions: &mut Vec<Action>) {
        let Some(actor) = state.actor(slot) else {
            return;
        };
        let spawning = |child: Slot| {
            actor.flights.values().any(|flight| {
                matches!(
                    flight.job,
                    Job::Deliver {
                        to: Some(to),
                        what: Delivery::Spawn,
                        ..
                    } if to == child
                )
            })
        };
        for (id, flight) in &actor.flights {
            if let Job::Deliver {
                to: Some(to),
                what,
                landing: Landing::Create | Landing::Born | Landing::Queued,
                ..
            } = flight.job
                && state
                    .actor(to)
                    .is_some_and(|recipient| !recipient.persisting())
                && (what != Delivery::Stop
                    || !spawning(to)
                    || self.bug == Some(Bug::StopBeforeBirth))
            {
                actions.push(Action::Land(slot, *id));
            }
        }
    }

    /// The user's commands to the actor at `slot`, delivered when its
    /// shell takes input.
    fn user_actions(&self, state: &World, slot: Slot, actions: &mut Vec<Action>) {
        let Some(actor) = state.actor(slot) else {
            return;
        };
        match &self.domain {
            Domain::Bounded(bounded) => {
                let core = self.core(state, slot);
                if slot == 0
                    && let Some(core) = &core
                {
                    // A message starts a turn, or steers the running one
                    // (up to the bound); a repeat the core decides
                    // (`Exists`).
                    if usize::from(state.sent) <= bounded.messages {
                        for index in 0..bounded.messages {
                            let id = message_id(index);
                            if core.running().is_none()
                                || delivered(actor, EnvelopeId::User(id))
                                || state.steers < bounded.steers
                            {
                                actions.push(Action::Deliver(
                                    slot,
                                    sized(self.born.actor, id, bounded.message_bytes),
                                ));
                            }
                        }
                    }
                    if state.stops < bounded.stops && core.born().is_some() && !core.stopped() {
                        actions.push(Action::Deliver(
                            slot,
                            stop(self.born.actor, stop_id(state.stops)),
                        ));
                    }
                    // Between turns (one while a turn runs is refused, as
                    // the core's tests check).
                    if state.compacts < bounded.compacts
                        && core.born().is_some()
                        && core.running().is_none()
                    {
                        actions.push(Action::Deliver(slot, Event::Compact));
                    }
                }
                let answers = |call: &CallId, actions: &mut Vec<Action>| {
                    for verdict in [Verdict::Approve, Verdict::Deny] {
                        actions.push(Action::Deliver(
                            slot,
                            Event::Approve {
                                call: call.clone(),
                                verdict,
                            },
                        ));
                    }
                };
                for call in &actor.asks {
                    answers(call, actions);
                }
                if state.stale_answers < bounded.stale_answers {
                    for call in self.audit(actor).proposed {
                        if !actor.asks.contains(&call) {
                            answers(&call, actions);
                        }
                    }
                }
                if state.interrupts < bounded.max_interrupts
                    && let Some(turn) = core.and_then(|core| core.running())
                {
                    actions.push(Action::Deliver(slot, Event::Interrupt { turn }));
                }
            }
            Domain::Trace(events) => actions.extend(
                events
                    .iter()
                    .filter(|(at, event)| {
                        *at == slot
                            && matches!(
                                event,
                                Event::Deliver {
                                    envelope: Envelope {
                                        from: Address::User,
                                        ..
                                    }
                                } | Event::Approve { .. }
                                    | Event::Interrupt { .. }
                                    | Event::Compact
                            )
                    })
                    .map(|(at, event)| Action::Deliver(*at, event.clone())),
            ),
        }
    }

    /// The replies effect `id` of the actor at `slot`, in flight, may make,
    /// and its deadline.
    fn reply_actions(
        &self,
        state: &World,
        slot: Slot,
        id: EffectId,
        flight: &Flight,
        actions: &mut Vec<Action>,
    ) {
        let durable = self.acknowledged(state, &flight.job);
        let admitted = |event: &Event<Abstract>| admits(id, flight, durable, event);
        match &self.domain {
            Domain::Bounded(bounded) => {
                actions.extend(
                    self.replies(bounded, state, slot, id, &flight.job)
                        .into_iter()
                        .filter(admitted)
                        .map(|event| Action::Deliver(slot, event)),
                );
                // A stop has no deadline: it is sent until it lands.
                let stop = matches!(
                    flight.job,
                    Job::Deliver {
                        what: Delivery::Stop,
                        ..
                    }
                );
                if bounded.expiries
                    && self.bug != Some(Bug::NoDeadline)
                    && matches!(flight.job, Job::Tool { .. } | Job::Deliver { .. })
                    && (!stop || self.bug == Some(Bug::StopExpires))
                {
                    actions.push(Action::Deliver(slot, Event::Expired { id }));
                }
            }
            Domain::Trace(events) => actions.extend(
                events
                    .iter()
                    .filter(|(at, event)| {
                        *at == slot
                            && (admitted(event)
                                || matches!(event, Event::Expired { id: expired } if *expired == id))
                    })
                    .map(|(at, event)| Action::Deliver(*at, event.clone())),
            ),
        }
    }

    /// Whether the append a delivery made is acknowledged to its sender:
    /// once it is durable in the recipient's log (under `AckBeforeDurable`,
    /// at once; under `BlockingSend`, once the recipient's turn that read
    /// it ended).
    fn acknowledged(&self, state: &World, job: &Job) -> bool {
        let Job::Deliver {
            to: Some(to),
            envelope,
            ..
        } = job
        else {
            return false;
        };
        let Some(recipient) = state.actor(*to) else {
            return false;
        };
        match self.bug {
            Some(Bug::AckBeforeDurable) => true,
            Some(Bug::BlockingSend) => answered(&recipient.log, *envelope),
            _ => delivered(recipient, *envelope),
        }
    }

    /// The replies a bounded domain offers effect `id` of the actor at
    /// `slot`, doing `job`.
    fn replies(
        &self,
        bounded: &Bounded,
        state: &World,
        slot: Slot,
        id: EffectId,
        job: &Job,
    ) -> Vec<Event<Abstract>> {
        match job {
            Job::Model { batches } => {
                let mut replies = Vec::new();
                if *batches < bounded.batches {
                    replies.push(Event::ModelParts { id, parts: size(1) });
                }
                replies.extend(bounded.scripts(slot).iter().map(|script| Event::ModelDone {
                    id,
                    result: ModelExit::Finished(self.proposals(state, slot, id, script)),
                }));
                if bounded.faults {
                    replies.extend(
                        [
                            ModelFault::Overloaded,
                            ModelFault::Other,
                            ModelFault::Cancelled,
                        ]
                        .map(|cause| Event::ModelDone {
                            id,
                            result: ModelExit::Failed(cause),
                        }),
                    );
                } else {
                    // A stopped call still answers that it was stopped.
                    replies.push(Event::ModelDone {
                        id,
                        result: ModelExit::Failed(ModelFault::Cancelled),
                    });
                }
                replies
            }
            // A timer fires: it has nothing else to say.
            Job::Tool {
                tool: Tool::Wake, ..
            } => vec![Event::ToolDone {
                id,
                exit: Exit::Ok,
                output: size(1),
            }],
            Job::Tool { .. } => [
                Exit::Ok,
                Exit::Failed,
                Exit::NotStarted(Unstarted::Cancelled),
                Exit::Unknown,
            ]
            .map(|exit| Event::ToolDone {
                id,
                exit,
                output: size(1),
            })
            .to_vec(),
            Job::Sleep => vec![Event::Woke { id }],
            // A summary that makes room or one that does not, a failure,
            // and, once asked to stop, a stop.
            Job::Summarize => bounded
                .summaries
                .iter()
                .map(|bytes| Summary::Written(size(*bytes)))
                .chain([
                    Summary::Failed(ModelFault::Other),
                    Summary::Failed(ModelFault::Cancelled),
                ])
                .map(|summary| Event::Summarized { id, summary })
                .collect(),
            Job::Deliver { landing, .. } => {
                let exit = match landing {
                    Landing::Answered(exit) => *exit,
                    Landing::Create | Landing::Born | Landing::Queued | Landing::Pending => {
                        Exit::Ok
                    }
                };
                vec![Event::ToolDone {
                    id,
                    exit,
                    output: size(1),
                }]
            }
        }
    }

    /// The calls of a bounded `script` for model call `id` of the actor at
    /// `slot`: a spawn names the next slot's actor, a cancel the oldest
    /// running task.
    fn proposals(
        &self,
        state: &World,
        slot: Slot,
        id: EffectId,
        script: &[Shape],
    ) -> Vec<Proposal<Abstract>> {
        script
            .iter()
            .enumerate()
            .map(|(position, shape)| {
                let args = match shape {
                    Shape::Spawn { subject, .. } => Shape::Spawn {
                        subject: subject.clone(),
                        child: slot
                            .checked_add(1)
                            .and_then(|child| self.id(child))
                            .unwrap_or(self.born.actor),
                    },
                    Shape::Cancel { .. } => Shape::Cancel {
                        task: state
                            .actor(slot)
                            .and_then(|actor| self.audit(actor).running().next())
                            .unwrap_or(TaskId(0)),
                    },
                    Shape::Read { .. }
                    | Shape::Edit { .. }
                    | Shape::Bash { .. }
                    | Shape::Send { .. }
                    | Shape::Wake
                    | Shape::Tasks => shape.clone(),
                };
                Proposal {
                    id: CallId::parse(&format!("c{}_{}_{position}", id.epoch.0, id.seq))
                        .expect("a protocol id"),
                    request: Request::Run { args },
                }
            })
            .collect()
    }

    /// Where the next step of delivery `id` of the actor at `slot` lands,
    /// and the event it delivers there: a child's birth, or the envelope
    /// (none for a spawn's creation of its child's log).
    #[must_use]
    pub fn landing(
        &self,
        world: &World,
        slot: Slot,
        id: EffectId,
    ) -> Option<(Slot, Option<Event<Abstract>>)> {
        let flight = world.actor(slot)?.flights.get(&id)?;
        let Job::Deliver {
            to: Some(to),
            landing,
            ..
        } = &flight.job
        else {
            return None;
        };
        let event = match landing {
            Landing::Create => None,
            Landing::Born => Some(Event::Born {
                born: self.child(world, slot, *to)?,
            }),
            Landing::Queued => Some(Event::Deliver {
                envelope: self.envelope(slot, *to, &flight.job)?,
            }),
            Landing::Pending | Landing::Answered(_) => return None,
        };
        Some((*to, event))
    }

    /// The step a delivery of the actor at `slot` takes into its
    /// recipient's log.
    fn land(&self, world: &mut World, slot: Slot, id: EffectId) -> Option<()> {
        let (to, event) = self.landing(world, slot, id)?;
        if world.actor(to)?.persisting() {
            return None;
        }
        let refused = |rejection: Rejection, stop: bool| match rejection {
            Rejection::Limit => Exit::NotStarted(Unstarted::Limit),
            // A stop to an actor already stopped has done its work.
            Rejection::Stopped if stop => Exit::Ok,
            Rejection::Stopped
            | Rejection::Unborn
            | Rejection::Mismatch
            | Rejection::UnknownCall
            | Rejection::NotRunning
            | Rejection::Running => Exit::NotStarted(Unstarted::Refused),
        };
        let next = match event {
            None => {
                let created = world.actors.iter().filter(|actor| actor.created).count();
                let cap = match &self.domain {
                    Domain::Bounded(bounded) => bounded.actors,
                    Domain::Trace(_) => usize::MAX,
                };
                if world.actor(to)?.created {
                    Landing::Born
                } else if created >= cap && self.bug != Some(Bug::NoActorLimit) {
                    Landing::Answered(Exit::NotStarted(Unstarted::Limit))
                } else {
                    world.actors.get_mut(to)?.created = true;
                    let boot = Core::<Abstract>::new(self.limits.clone())
                        .resume(None, Some(self.loaded()));
                    self.stage(world, to, &boot, Some(0));
                    Landing::Born
                }
            }
            Some(born @ Event::Born { .. }) => match self.deliver(world, to, born)? {
                Outcome::Accepted | Outcome::Exists => Landing::Queued,
                Outcome::Rejected(rejection) => Landing::Answered(refused(rejection, false)),
                Outcome::Stale => Landing::Answered(Exit::NotStarted(Unstarted::Refused)),
            },
            Some(Event::Deliver { envelope }) => {
                let stop = matches!(envelope.body, Body::Stop);
                let id_of = envelope.id;
                let (call, what) = match &world.actor(slot)?.flights.get(&id)?.job {
                    Job::Deliver { call, what, .. } => (call.clone(), *what),
                    Job::Model { .. } | Job::Tool { .. } | Job::Sleep | Job::Summarize => {
                        return None;
                    }
                };
                match self.deliver(world, to, Event::Deliver { envelope })? {
                    Outcome::Accepted => {
                        if let Some(call) = call {
                            world.carried_out.insert((to, id_of), (slot, call));
                        }
                        Landing::Pending
                    }
                    Outcome::Exists => Landing::Answered(Exit::Ok),
                    // A spawn made its child, so it acted (T3): a stopped
                    // or full child is `Ok`, said so in its output.
                    Outcome::Rejected(Rejection::Stopped | Rejection::Limit)
                        if what == Delivery::Spawn =>
                    {
                        Landing::Answered(Exit::Ok)
                    }
                    Outcome::Rejected(_) | Outcome::Stale if what == Delivery::Spawn => {
                        Landing::Answered(Exit::Unknown)
                    }
                    Outcome::Rejected(rejection) => Landing::Answered(refused(rejection, stop)),
                    Outcome::Stale => Landing::Answered(Exit::NotStarted(Unstarted::Refused)),
                }
            }
            Some(_) => return None,
        };
        if let Some(Flight {
            job: Job::Deliver { landing, .. },
            ..
        }) = world.actors.get_mut(slot)?.flights.get_mut(&id)
        {
            *landing = next;
        }
        Some(())
    }

    /// The daemon crashes: each actor keeps `keep` of its pending records,
    /// and then each resumes.
    fn crash(&self, world: &mut World, keep: &[usize]) -> Option<()> {
        world.crashes = world.crashes.saturating_add(1);
        // The client's connection dies with the daemon.
        world.client.cursor = None;
        if keep.len() != world.actors.len() {
            return None;
        }
        for (actor, keep) in world.actors.iter_mut().zip(keep) {
            let survived = actor.pending.get(..*keep)?.to_vec();
            actor.log.extend(survived);
            actor.pending.clear();
            actor.staged.clear();
            actor.asks.clear();
            actor.refused = false;
            // Only tools outlive the process (a child may still run);
            // model streams, timers and the outbox die with it.
            let flights = std::mem::take(&mut actor.flights);
            actor.orphans.extend(
                flights
                    .into_iter()
                    .filter(|(_, flight)| matches!(flight.job, Job::Tool { .. })),
            );
        }
        for slot in 0..world.actors.len() {
            if world.actor(slot).is_some_and(|actor| actor.created) {
                self.resume(world, slot);
            }
        }
        Some(())
    }

    /// The actor at `slot` resumes after a crash.
    fn resume(&self, world: &mut World, slot: Slot) {
        let Some(core) = self.core(world, slot) else {
            return;
        };
        let from = world.actor(slot).map_or(0, |actor| actor.log.len());
        let mut resumed = core.resume(self.root(slot), self.load(&core));
        let truncate = |resumed: &mut Decision<Abstract>,
                        pick: &dyn Fn(&Record<Abstract>) -> bool| {
            if let Some(at) = resumed.records.iter().position(pick) {
                resumed.records.truncate(at);
                resumed.effects.clear();
            }
        };
        match self.bug {
            Some(Bug::LeaveDispatched) => truncate(&mut resumed, &|record| {
                matches!(
                    record,
                    Record::ToolResult {
                        exit: Exit::Unknown,
                        ..
                    }
                )
            }),
            Some(Bug::LeaveTaskRunning) => truncate(&mut resumed, &|record| {
                matches!(
                    record,
                    Record::TaskEnded {
                        exit: Exit::Unknown,
                        ..
                    }
                )
            }),
            Some(Bug::EndTaskTwice) => {
                // The resume ends again the first task that already ended.
                let ended = world.actor(slot).and_then(|actor| {
                    actor.log.iter().find_map(|record| {
                        if let Record::TaskEnded { task, .. } = record {
                            Some(*task)
                        } else {
                            None
                        }
                    })
                });
                if let Some(task) = ended {
                    resumed.records.push(Record::TaskEnded {
                        task,
                        exit: Exit::Unknown,
                        output: None,
                    });
                }
            }
            _ => {}
        }
        self.stage(world, slot, &resumed, Some(from));
        let Some(actor) = world.actors.get_mut(slot) else {
            return;
        };
        if self.bug == Some(Bug::UnsafeResolvedNotStarted) {
            for record in &mut actor.pending {
                if let Record::ToolResult { exit, .. } = record
                    && *exit == Exit::Unknown
                {
                    *exit = Exit::NotStarted(Unstarted::Cancelled);
                }
            }
        }
        if self.bug == Some(Bug::RerunUnsafe) {
            let epoch = core.epoch().0.saturating_add(1);
            for (id, flight) in &actor.orphans {
                if let Job::Tool { tool, .. } = flight.job
                    && tool.class() == Class::Unsafe
                {
                    actor.staged.push(Staged::Start(
                        EffectId {
                            epoch: Epoch(epoch),
                            seq: id.seq,
                        },
                        flight.job.clone(),
                    ));
                }
            }
        }
    }

    /// A user's message is spliced into the running step of the actor at
    /// `slot` while its calls are unresolved (`Bug::SpliceSteering`).
    fn splice(&self, world: &mut World, slot: Slot, id: EnvelopeId) {
        let Some(actor) = world.actor(slot) else {
            return;
        };
        let mut log = actor.log.clone();
        log.extend(actor.pending.iter().cloned());
        let open = audit(&log, &self.limits);
        let step = log.iter().rev().find_map(|record| {
            if let Record::StepStarted { step, .. } = record {
                Some(*step)
            } else {
                None
            }
        });
        let dispatched = log
            .iter()
            .rev()
            .take_while(|record| !matches!(record, Record::StepStarted { .. }))
            .any(|record| matches!(record, Record::Dispatched { .. }));
        if let (Some(step), true, false) = (step, dispatched, open.stopped)
            && let Some(actor) = world.actors.get_mut(slot)
        {
            actor.pending.push(Record::StepStarted {
                step: StepId {
                    turn: step.turn,
                    index: step.index.saturating_add(1),
                },
                carries: vec![id],
            });
        }
    }

    /// Whether the world has settled: every actor quiescent, and nothing
    /// left for the user to do.
    fn settled(&self, world: &World) -> bool {
        if !world.quiescent() {
            return false;
        }
        let mut actions = Vec::new();
        for slot in 0..world.actors.len() {
            self.user_actions(world, slot, &mut actions);
        }
        actions.is_empty()
    }
}

impl Flight {
    /// A flight just started: a tool's run has yet to act (a timer only
    /// waits, so it never does).
    const fn new(job: Job) -> Self {
        Self {
            executed: !matches!(
                job,
                Job::Tool {
                    tool: Tool::Read
                        | Tool::Edit
                        | Tool::Bash
                        | Tool::Spawn
                        | Tool::Send
                        | Tool::Tasks
                        | Tool::Cancel,
                    ..
                }
            ),
            job,
            cancelled: false,
            hung: false,
        }
    }
}

/// Whether the durable log of `actor` holds envelope `id`.
fn delivered(actor: &Actor, id: EnvelopeId) -> bool {
    actor.log.iter().any(|record| {
        matches!(
            record,
            Record::Delivered { envelope } if envelope.id == id
        )
    })
}

/// Whether a turn that carried envelope `id` has ended in `log`.
fn answered(log: &[Rec], id: EnvelopeId) -> bool {
    let carried = log.iter().find_map(|record| {
        if let Record::StepStarted { step, carries } = record
            && carries.contains(&id)
        {
            Some(step.turn)
        } else {
            None
        }
    });
    carried.is_some_and(|turn| {
        log.iter()
            .any(|record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn))
    })
}

impl Model for Harness {
    type State = World;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![self.boot()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for slot in 0..state.actors.len() {
            let Some(actor) = state.actor(slot) else {
                continue;
            };
            if !actor.created {
                continue;
            }
            if actor.persisting() {
                // The shell persists the decision before it delivers
                // anything more to the core; meanwhile tools run, other
                // actors go on, and clients read.
                actions.push(Action::Sync(slot));
            } else {
                self.user_actions(state, slot, actions);
                for (id, flight) in &actor.flights {
                    self.reply_actions(state, slot, *id, flight, actions);
                }
                if let Domain::Bounded(bounded) = &self.domain {
                    for id in actor.orphans.keys() {
                        actions.push(Action::OrphanComplete(slot, *id));
                    }
                    if state.duplicates < bounded.max_duplicates && actor.last.is_some() {
                        actions.push(Action::Duplicate(slot));
                    }
                }
            }
            self.executions(state, slot, actions);
            self.landings(state, slot, actions);
        }
        if self.may_crash(state) {
            // A crash keeps a prefix of every pending group.
            let mut keeps: Vec<Vec<usize>> = vec![Vec::new()];
            for actor in &state.actors {
                keeps = keeps
                    .into_iter()
                    .flat_map(|keep| {
                        (0..=actor.pending.len()).map(move |kept| {
                            let mut keep = keep.clone();
                            keep.push(kept);
                            keep
                        })
                    })
                    .collect();
            }
            actions.extend(keeps.into_iter().map(Action::Crash));
        }
        self.client_actions(state, actions);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per action, each the world's step"
    )]
    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut world = last_state.clone();
        match action {
            Action::Sync(slot) => Self::sync(&mut world, slot),
            Action::Crash(keep) => self.crash(&mut world, &keep)?,
            Action::Land(slot, id) => self.land(&mut world, slot, id)?,
            Action::Hang(slot, id) => {
                world.hangs = world.hangs.saturating_add(1);
                world.actors.get_mut(slot)?.flights.get_mut(&id)?.hung = true;
            }
            Action::Deliver(slot, event) => {
                let running = self
                    .core(&world, slot)
                    .is_some_and(|core| core.running().is_some());
                let mut steered = None;
                match &event {
                    Event::Deliver { envelope } => match envelope.body {
                        Body::Text(_) => {
                            world.sent = world.sent.saturating_add(1);
                            if running && !delivered(world.actor(slot)?, envelope.id) {
                                world.steers = world.steers.saturating_add(1);
                                steered = Some(envelope.id);
                            }
                        }
                        Body::Stop => world.stops = world.stops.saturating_add(1),
                    },
                    Event::Approve { call, verdict } => {
                        let actor = world.actors.get_mut(slot)?;
                        if *verdict == Verdict::Approve {
                            actor.approved.insert(call.clone());
                        }
                        if !actor.asks.remove(call) {
                            world.stale_answers = world.stale_answers.saturating_add(1);
                        }
                    }
                    Event::Interrupt { .. } => {
                        world.interrupts = world.interrupts.saturating_add(1);
                    }
                    Event::Compact => world.compacts = world.compacts.saturating_add(1),
                    Event::Expired { id } => {
                        // v stops waiting; the run may still act and report,
                        // as an orphan.
                        let actor = world.actors.get_mut(slot)?;
                        if let Some(flight) = actor.flights.remove(id) {
                            actor.orphans.insert(*id, flight);
                        }
                    }
                    // Births and rules are the boot's, never a world's
                    // action.
                    Event::Born { .. } | Event::Rules { .. } => {}
                    Event::ModelParts { id, .. } => {
                        let Job::Model { batches } =
                            &mut world.actors.get_mut(slot)?.flights.get_mut(id)?.job
                        else {
                            return None;
                        };
                        *batches = batches.saturating_add(1);
                    }
                    Event::ToolDone { id, exit, .. } => {
                        let flight = world.actors.get_mut(slot)?.flights.remove(id)?;
                        if let (
                            Job::Deliver {
                                to: Some(to),
                                envelope,
                                ..
                            },
                            Exit::Ok,
                        ) = (&flight.job, exit)
                        {
                            world.acked.insert((*to, *envelope));
                        }
                    }
                    Event::ModelDone { id, .. }
                    | Event::Woke { id }
                    | Event::Summarized { id, .. } => {
                        world.actors.get_mut(slot)?.flights.remove(id)?;
                    }
                }
                self.deliver(&mut world, slot, event);
                if self.bug == Some(Bug::SpliceSteering)
                    && let Some(id) = steered
                {
                    self.splice(&mut world, slot, id);
                }
            }
            Action::Execute(slot, id) => {
                let actor = world.actors.get_mut(slot)?;
                let flight = actor.flights.get_mut(&id)?;
                flight.executed = true;
                let job = flight.job.clone();
                Self::run(actor, &job);
            }
            Action::OrphanExecute(slot, id) => {
                let actor = world.actors.get_mut(slot)?;
                let flight = actor.orphans.get_mut(&id)?;
                flight.executed = true;
                let job = flight.job.clone();
                Self::run(actor, &job);
            }
            Action::OrphanComplete(slot, id) => {
                let flight = world.actors.get_mut(slot)?.orphans.remove(&id)?;
                self.deliver_stale(&mut world, slot, Self::reply(id, &flight));
            }
            Action::Duplicate(slot) => {
                world.duplicates = world.duplicates.saturating_add(1);
                let event = world.actor(slot)?.last.clone()?;
                self.deliver_stale(&mut world, slot, event);
            }
            Action::Read => {
                let cursor = world.client.cursor?;
                let record = self.served(&world).get(cursor)?.clone();
                world.client.seen.push(record);
                world.client.cursor = Some(cursor.saturating_add(1));
                world.client.next = cursor.saturating_add(1);
            }
            Action::Reconnect => {
                if world.client.cursor.is_some() {
                    world.client.reconnects = world.client.reconnects.saturating_add(1);
                }
                // A new connection reads from the client's own offset.
                let from = if self.bug == Some(Bug::SkipOnReconnect) {
                    world.client.next.saturating_add(1)
                } else {
                    world.client.next
                };
                world.client.cursor = Some(from);
                if from > 0 {
                    world.client.resumed_at = Some(from);
                }
            }
        }
        Some(world)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the list of properties, each named by its clause"
    )]
    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // C2. Runs are no more than starts: a run acts only in a flight.
            Property::always(
                "an Unsafe call is dispatched at most once across crashes",
                |_, world: &World| {
                    world.actors.iter().all(|actor| {
                        actor
                            .starts
                            .values()
                            .all(|(tool, count)| tool.class() != Class::Unsafe || *count <= 1)
                    })
                },
            ),
            // T3.
            Property::always(
                "after a crash a dispatched Unsafe call resolves Unknown",
                |model: &Self, world: &World| {
                    !breached(model, world, Breach::UnsafeKnownAfterCrash)
                },
            ),
            // State is the replay. Every log shape the core refuses (`spec_apply`) is caught
            // here.
            Property::always(
                "every durable prefix recovers",
                |model: &Self, world: &World| {
                    (0..world.actors.len()).all(|slot| model.core(world, slot).is_some())
                },
            ),
            // T2.
            Property::always(
                "the client sees a prefix of the durable log",
                |_, world: &World| {
                    world.client.next == world.client.seen.len()
                        && world.root().log.get(..world.client.seen.len())
                            == Some(world.client.seen.as_slice())
                },
            ),
            Property::always("stale completions change nothing", |_, world: &World| {
                !world.stale_changed
            }),
            // C1.
            Property::always(
                "every grant is the user's or a rule in force that fits",
                |model: &Self, world: &World| !breached(model, world, Breach::GrantUnfit),
            ),
            // C1: a user's grant answers the user's command.
            Property::always(
                "every user's grant answers the user's approval command",
                |model: &Self, world: &World| {
                    world
                        .actors
                        .iter()
                        .all(|actor| model.audit(actor).user_grants.is_subset(&actor.approved))
                },
            ),
            Property::always(
                "calls past a bound never run",
                |model: &Self, world: &World| !breached(model, world, Breach::OverBoundDispatched),
            ),
            // T3, held: a result for every call at quiescence.
            Property::always(
                "quiescent implies the turn ended",
                |model: &Self, world: &World| {
                    !world.quiescent()
                        || (0..world.actors.len()).all(|slot| {
                            model
                                .core(world, slot)
                                .is_some_and(|core| core.running().is_none())
                        })
                },
            ),
            // C1, across logs: an envelope minted by an actor lands only
            // after the record that licenses it is durable in the sender's
            // log.
            Property::always(
                "every delivery follows its durable license",
                |model: &Self, world: &World| licensed(model, world),
            ),
            // C2: an acknowledged delivery took effect, once.
            Property::always(
                "an acknowledged delivery is in its recipient's durable log once",
                |_, world: &World| {
                    world.acked.iter().all(|(to, id)| {
                        world.actor(*to).is_some_and(|recipient| {
                            recipient
                                .log
                                .iter()
                                .filter(|record| matches!(record, Record::Delivered { envelope } if envelope.id == *id))
                                .count()
                                == 1
                        })
                    })
                },
            ),
            // C2: a `Keyed` call takes effect at most once, whatever the id
            // it is sent under.
            Property::always("no call is delivered twice", |_, world: &World| once(world)),
            // T3.
            Property::always(
                "every call and task resolves at most once",
                |model: &Self, world: &World| !breached(model, world, Breach::ResolvedTwice),
            ),
            // T3 and T4: every task ends, so its actor is notified.
            Property::always(
                "at quiescence every task has ended",
                |model: &Self, world: &World| {
                    !world.quiescent()
                        || model
                            .audits(world)
                            .all(|audit| audit.running().next().is_none())
                },
            ),
            // T4: at most once.
            Property::always(
                "every envelope is carried at most once",
                |model: &Self, world: &World| !breached(model, world, Breach::CarriedTwice),
            ),
            // T4: at least once, unless the actor stopped or ran out of
            // turns or context first.
            Property::always(
                "at quiescence every envelope is carried",
                |model: &Self, world: &World| {
                    !world.quiescent()
                        || model
                            .audits(world)
                            .all(|audit| audit.read_all(&model.limits))
                },
            ),
            // T4.
            Property::always(
                "mail is carried only at a step boundary",
                |model: &Self, world: &World| !breached(model, world, Breach::Spliced),
            ),
            // P1.
            Property::always(
                "every actor stays within its limits",
                |model: &Self, world: &World| {
                    let cap = match &model.domain {
                        Domain::Bounded(bounded) => bounded.actors,
                        Domain::Trace(_) => usize::MAX,
                    };
                    !breached(model, world, Breach::OverLimit)
                        && world.actors.iter().filter(|actor| actor.created).count() <= cap
                },
            ),
            // P1: no actor waits on another, every run resolves by
            // its deadline, and without new input the world goes quiet.
            // Every path is finite (the world's faults and the user's
            // input are bounded), so this is checked on the states where a
            // path ends: fairness by finiteness.
            // C2 for a stop: it is `Keyed`, so it reaches the child. Once
            // the world settles, every child a stopped actor spawned is
            // stopped too (an unborn one never acts).
            Property::always(
                "every stop reaches its child",
                |model: &Self, world: &World| {
                    !model.settled(world)
                        || (0..world.actors.len()).all(|slot| {
                            let Some(core) = model.core(world, slot) else {
                                return true;
                            };
                            let parent = core
                                .born()
                                .and_then(|born| born.parent)
                                .and_then(|parent| model.slot_of(parent))
                                .and_then(|parent| model.core(world, parent));
                            parent.is_none_or(|parent| !parent.stopped() || core.stopped())
                        })
                },
            ),
            Property::eventually("the world settles", |model: &Self, world: &World| {
                model.settled(world)
            }),
            Property::sometimes("a tool runs", |_, world: &World| {
                world.actors.iter().any(|actor| !actor.runs.is_empty())
            }),
            Property::sometimes("a turn completes", |_, world: &World| {
                logged(world, |record| {
                    matches!(
                        record,
                        Record::TurnEnded {
                            outcome: TurnOutcome::Completed,
                            ..
                        }
                    )
                })
            }),
            Property::sometimes("a turn is cancelled", |_, world: &World| {
                logged(world, |record| {
                    matches!(
                        record,
                        Record::TurnEnded {
                            outcome: TurnOutcome::Cancelled,
                            ..
                        }
                    )
                })
            }),
            // v resolves it `Unknown` (a crash), not a run's report.
            Property::sometimes("an outcome is unknown", |_, world: &World| {
                logged(world, |record| {
                    matches!(
                        record,
                        Record::ToolResult {
                            exit: Exit::Unknown,
                            output: None,
                            ..
                        }
                    )
                })
            }),
            Property::sometimes("a step is retried", |_, world: &World| {
                logged(world, |record| {
                    matches!(
                        record,
                        Record::StepReset {
                            reason: verified_domain::Reset::Retry { .. },
                            ..
                        }
                    )
                })
            }),
            Property::sometimes("a stale completion is refused", |_, world: &World| {
                world.stale_refused
            }),
            Property::sometimes("a call not awaiting one is answered", |_, world: &World| {
                world.stale_answers > 0
            }),
            Property::sometimes(
                "a turn is cancelled after its model finished",
                |model: &Self, world: &World| {
                    model
                        .audits(world)
                        .any(|audit| audit.cancelled_after_finish.is_some())
                },
            ),
            Property::sometimes("a call fails by a limit", |model: &Self, world: &World| {
                model.audits(world).any(|audit| audit.limited > 0)
            }),
            Property::sometimes("two Unsafe calls run at once", |_, world: &World| {
                world.actors.iter().any(|actor| {
                    actor
                        .flights
                        .values()
                        .filter(|flight| {
                            flight.executed
                                && matches!(
                                    flight.job,
                                    Job::Tool { tool, .. } if tool.class() == Class::Unsafe
                                )
                        })
                        .count()
                        >= 2
                })
            }),
            Property::sometimes("a Safe call reruns after a crash", |_, world: &World| {
                world.actors.iter().any(|actor| {
                    actor
                        .starts
                        .values()
                        .any(|(tool, count)| tool.class() == Class::Safe && *count >= 2)
                })
            }),
            Property::sometimes("a model call streams a second batch", |_, world: &World| {
                world.actors.iter().any(|actor| {
                    actor
                        .flights
                        .values()
                        .any(|flight| matches!(flight.job, Job::Model { batches } if batches >= 2))
                })
            }),
            Property::sometimes("the client catches up", |_, world: &World| {
                let root = world.root();
                world.quiescent()
                    && world.client.seen.len() == root.log.len()
                    && root
                        .log
                        .iter()
                        .any(|record| matches!(record, Record::TurnEnded { .. }))
            }),
            Property::sometimes(
                "a client resumed mid-stream catches up",
                |_, world: &World| {
                    let root = world.root();
                    world.quiescent()
                        && world.client.resumed_at.is_some_and(|from| {
                            root.log
                                .iter()
                                .take(from)
                                .any(|record| matches!(record, Record::TurnStarted { .. }))
                        })
                        && world.client.seen.len() == root.log.len()
                },
            ),
            Property::sometimes("an envelope is acknowledged", |_, world: &World| {
                !world.acked.is_empty()
            }),
            Property::sometimes("a child is spawned", |_, world: &World| {
                world.actors.iter().skip(1).any(|actor| {
                    actor.log.iter().any(
                        |record| matches!(record, Record::Born { born } if born.parent.is_some()),
                    )
                })
            }),
            Property::sometimes("a task ends", |_, world: &World| {
                logged(world, |record| matches!(record, Record::TaskEnded { .. }))
            }),
            Property::sometimes(
                "a step carries two notifications",
                |model: &Self, world: &World| model.audits(world).any(|audit| audit.coalesced),
            ),
            Property::sometimes("a call is denied by a steer", |_, world: &World| {
                logged(world, |record| {
                    matches!(
                        record,
                        Record::ToolResult {
                            exit: Exit::NotStarted(Unstarted::Steered),
                            ..
                        }
                    )
                })
            }),
            Property::sometimes("a steered message lands in its turn", |_, world: &World| {
                logged(world, |record| {
                    matches!(
                        record,
                        Record::StepStarted { step, carries }
                            if step.index > 0
                                && carries.iter().any(|id| matches!(id, EnvelopeId::User(_)))
                    )
                })
            }),
            Property::sometimes("a stop reaches a child", |_, world: &World| {
                world.actors.iter().skip(1).any(|actor| {
                    actor.log.iter().any(|record| matches!(record, Record::Delivered { envelope } if matches!(envelope.body, Body::Stop) && envelope.from == Address::Runtime))
                })
            }),
            Property::sometimes("a run resolves at its deadline", |_, world: &World| {
                world.hangs > 0
                    && logged(world, |record| {
                        matches!(
                            record,
                            Record::ToolResult {
                                exit: Exit::Unknown,
                                output: None,
                                ..
                            } | Record::TaskEnded {
                                exit: Exit::Unknown,
                                output: None,
                                ..
                            }
                        )
                    })
            }),
            Property::sometimes(
                "a delivery lands again after a crash",
                |_, world: &World| world.crashes > 0 && !world.acked.is_empty(),
            ),
            Property::sometimes(
                "a forced compaction lets its turn go on",
                |model: &Self, world: &World| {
                    model.audits(world).any(|audit| audit.compactions.went_on)
                },
            ),
            Property::sometimes("an idle compaction lands", |model: &Self, world: &World| {
                model.audits(world).any(|audit| audit.compactions.landed)
            }),
            Property::sometimes(
                "a compaction is superseded",
                |model: &Self, world: &World| {
                    model
                        .audits(world)
                        .any(|audit| audit.compactions.superseded)
                },
            ),
        ]
    }
}

/// Whether some actor's log breaks `breach`.
fn breached(model: &Harness, world: &World, breach: Breach) -> bool {
    model
        .audits(world)
        .any(|audit| audit.breaches.contains(&breach))
}

/// Whether some actor's durable log holds a record `pick` chooses.
fn logged(world: &World, pick: impl Fn(&Rec) -> bool) -> bool {
    world.actors.iter().any(|actor| actor.log.iter().any(&pick))
}

/// Whether every envelope minted from an actor's log follows its license,
/// durable in that log: a `Keyed` call's `Dispatched` or a `StopSent`, at
/// the envelope's seq.
fn licensed(model: &Harness, world: &World) -> bool {
    let licenses: BTreeMap<ActorId, BTreeSet<u64>> = world
        .actors
        .iter()
        .enumerate()
        .filter_map(|(slot, actor)| Some((model.id(slot)?, model.audit(actor).licenses)))
        .collect();
    world.actors.iter().all(|actor| {
        actor.log.iter().chain(&actor.pending).all(|record| {
            let Record::Delivered {
                envelope:
                    Envelope {
                        id: EnvelopeId::Minted { actor, seq },
                        ..
                    },
            } = record
            else {
                return true;
            };
            licenses
                .get(actor)
                .is_some_and(|licenses| licenses.contains(seq))
        })
    })
}

/// Whether every call took effect at most once: no two envelopes durable
/// in recipients' logs carry out the same call.
fn once(world: &World) -> bool {
    let mut seen: BTreeSet<(Slot, CallId)> = BTreeSet::new();
    world.actors.iter().enumerate().all(|(to, actor)| {
        actor.log.iter().all(|record| {
            let Record::Delivered { envelope } = record else {
                return true;
            };
            world
                .carried_out
                .get(&(to, envelope.id))
                .is_none_or(|call| seen.insert(call.clone()))
        })
    })
}
