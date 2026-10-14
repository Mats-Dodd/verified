//! The model: the real core inside an abstract world, its actions, and the
//! properties every reachable state must keep.

use stateright::{Model, Property};
use verified_core::{Core, Decision, Event, ModelExit, Outcome, Verdict};
use verified_domain::{
    Abstract, Approver, CallId, EffectId, Epoch, Exit, Limits, Measured, MessageId, ModelFault,
    Proposal, Record, Request, Rerun, Reset, Tool, TurnId, TurnOutcome, Unit, UnixMillis,
    Unstarted,
};

use crate::alpha::{self, Work};
use crate::audit::{Audit, Breach, audit};
use crate::world::{Flight, Job, Rec, Staged, World};

/// One step of the world.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    /// An event enters the core: a user's command, or a reply of an effect
    /// in flight. It carries the event's values (ids, sizes, a model's
    /// proposals), so a trace's events are actions as they are; which
    /// values are offered is the [`Domain`]'s.
    Deliver(Event<Abstract>),
    /// A tool's side effect happens: the shell's `ToolRan` point, where its
    /// run has returned.
    Execute(EffectId),
    OrphanExecute(EffectId),
    OrphanComplete(EffectId),
    Duplicate,
    /// The decision being persisted becomes durable and its effects start.
    Sync,
    /// The daemon crashes, keeping this many of the pending records.
    Crash(usize),
    /// The client reads the next committed record on its connection.
    Read,
    /// The client opens a new connection, resuming from its offset.
    Reconnect,
}

/// The kind of an [`Action`], without its values: a delivery by its
/// event's.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Send,
    Answer,
    Interrupt,
    Execute,
    Parts,
    Finish,
    Fail,
    ToolDone,
    Woke,
    OrphanExecute,
    OrphanComplete,
    Duplicate,
    Sync,
    Crash,
    Read,
    Reconnect,
}

impl Action {
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Deliver(Event::Message { .. }) => Kind::Send,
            Self::Deliver(Event::Approve { .. }) => Kind::Answer,
            Self::Deliver(Event::Interrupt { .. }) => Kind::Interrupt,
            Self::Deliver(Event::ModelParts { .. }) => Kind::Parts,
            Self::Deliver(Event::ModelDone {
                result: ModelExit::Finished(_),
                ..
            }) => Kind::Finish,
            Self::Deliver(Event::ModelDone {
                result: ModelExit::Failed(_),
                ..
            }) => Kind::Fail,
            Self::Deliver(Event::ToolDone { .. }) => Kind::ToolDone,
            Self::Deliver(Event::Woke { .. }) => Kind::Woke,
            Self::Execute(_) => Kind::Execute,
            Self::OrphanExecute(_) => Kind::OrphanExecute,
            Self::OrphanComplete(_) => Kind::OrphanComplete,
            Self::Duplicate => Kind::Duplicate,
            Self::Sync => Kind::Sync,
            Self::Crash(_) => Kind::Crash,
            Self::Read => Kind::Read,
            Self::Reconnect => Kind::Reconnect,
        }
    }

    /// The event this action delivers, if it delivers one.
    #[must_use]
    pub const fn event(&self) -> Option<&Event<Abstract>> {
        match self {
            Self::Deliver(event) => Some(event),
            Self::Execute(_)
            | Self::OrphanExecute(_)
            | Self::OrphanComplete(_)
            | Self::Duplicate
            | Self::Sync
            | Self::Crash(_)
            | Self::Read
            | Self::Reconnect => None,
        }
    }
}

/// The values actions may carry.
#[derive(Clone, Debug)]
pub enum Domain {
    /// A small synthetic set, for BFS.
    Bounded(Bounded),
    /// The events a trace delivers (each once), to replay it.
    Trace(Vec<Event<Abstract>>),
}

/// The BFS's values: one-byte payloads, ids made from small indices, and
/// bounds on the world's faults.
#[derive(Clone, Debug)]
pub struct Bounded {
    /// Distinct messages the user may send; one extra send repeats the first.
    pub messages: usize,
    /// The model's possible responses, each the tools it proposes; call ids
    /// are made from the model call's id, unique per call as the
    /// provider's are.
    pub scripts: Vec<Vec<Tool>>,
    /// Batches of parts a model call may stream before it ends.
    pub batches: u8,
    /// Answers the user may give to a proposed call not awaiting one:
    /// repeated, late, racing another, or after a crash dropped the ask.
    pub stale_answers: u8,
    pub max_crashes: u8,
    pub max_interrupts: u8,
    pub max_duplicates: u8,
    /// Whether a client follows the stream (and may reconnect this often).
    pub client: Option<u8>,
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
    /// Policy approves a call of any tool, not only `Safe` ones.
    PolicyApprovesAll,
    /// After a crash, drop the `Unknown` result of a dispatched `Unsafe`
    /// call, and all the resume decided after it, so the call stays
    /// dispatched.
    LeaveDispatched,
}

/// The core under `limits` in an abstract world.
///
/// Its state is [`World`], and the core is always `recover(log)`: it has no
/// other state. The world boots as the daemon does, by persisting `resume()`
/// of the empty log.
#[derive(Clone, Debug)]
pub struct Harness {
    pub limits: Limits,
    pub domain: Domain,
    pub bug: Option<Bug>,
}

/// The ids of message `slot` of a bounded domain.
fn message_ids(slot: usize) -> (MessageId, TurnId) {
    let number = u64::try_from(slot).expect("few slots");
    let at = UnixMillis::new(1_700_000_000_000u64.saturating_add(number)).expect("a 48-bit time");
    (
        MessageId::from_parts(at, &[1; 10]),
        TurnId::from_parts(at, &[2; 10]),
    )
}

/// The calls of a bounded `script` for model call `id`.
fn proposals(id: EffectId, tools: &[Tool]) -> Vec<Proposal<Abstract>> {
    tools
        .iter()
        .enumerate()
        .map(|(position, tool)| Proposal {
            id: CallId::parse(&format!("c{}_{}_{position}", id.epoch.0, id.seq))
                .expect("a protocol id"),
            request: Request::Run { args: *tool },
        })
        .collect()
}

const fn size(bytes: u64) -> Measured<Unit> {
    Measured { body: Unit, bytes }
}

/// Whether `flight`, effect `id`, may reply `event`: a reply of its own
/// job's kind, a cancelled one only once it was asked to stop, and a tool's
/// other reports only once it ran. A cancelled run reports `NotStarted`
/// only if it never acted (H4); a run that acted and was stopped (a cancel,
/// or its own timeout) reports `Unknown`.
fn admits(id: EffectId, flight: &Flight, event: &Event<Abstract>) -> bool {
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
            matches!(flight.job, Job::Tool { .. })
                && match exit {
                    Exit::NotStarted(_) => flight.cancelled && !flight.executed,
                    // `Unknown`: a run stopped mid-way (a cancel, or its own
                    // timeout).
                    Exit::Ok | Exit::Failed | Exit::Unknown => flight.executed,
                },
        ),
        Event::Woke { id } => (id, matches!(flight.job, Job::Sleep)),
        Event::Message { .. } | Event::Approve { .. } | Event::Interrupt { .. } => return false,
    };
    *of == id && admitted
}

/// The replies a bounded domain offers effect `id` doing `job`.
fn replies(bounded: &Bounded, id: EffectId, job: &Job) -> Vec<Event<Abstract>> {
    match job {
        Job::Model { batches } => {
            let mut replies = Vec::new();
            if *batches < bounded.batches {
                replies.push(Event::ModelParts { id, parts: size(1) });
            }
            replies.extend(bounded.scripts.iter().map(|script| Event::ModelDone {
                id,
                result: ModelExit::Finished(proposals(id, script)),
            }));
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
            replies
        }
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
    }
}

impl Harness {
    /// A harness for BFS under `limits` over `bounded`.
    #[must_use]
    pub const fn bounded(limits: Limits, bounded: Bounded) -> Self {
        Self {
            limits,
            domain: Domain::Bounded(bounded),
            bug: None,
        }
    }

    /// A harness for replaying traces run under `limits`: its deliveries
    /// carry the values `events` do.
    #[must_use]
    pub fn trace<E: IntoIterator<Item = Event<Abstract>>>(limits: Limits, events: E) -> Self {
        let mut distinct: Vec<Event<Abstract>> = Vec::new();
        for event in events {
            if !distinct.contains(&event) {
                distinct.push(event);
            }
        }
        Self {
            limits,
            domain: Domain::Trace(distinct),
            bug: None,
        }
    }

    /// Whether the world may crash again.
    const fn may_crash(&self, world: &World) -> bool {
        match &self.domain {
            Domain::Bounded(bounded) => world.crashes < bounded.max_crashes,
            Domain::Trace(_) => true,
        }
    }

    fn audit(&self, world: &World) -> Audit {
        audit(&world.log, &self.limits)
    }

    pub(crate) fn core(&self, world: &World) -> Option<Core<Abstract>> {
        Core::recover(self.limits.clone(), &world.log).ok()
    }

    fn stage(&self, world: &mut World, decision: &Decision<Abstract>) {
        let decided = alpha::decision(decision);
        world.pending = decided.records;
        if self.bug == Some(Bug::PolicyApprovesAll) {
            // The shell approves what it should ask about, as if by policy.
            for work in &decided.effects {
                if let Work::AskApproval { call, .. } = work {
                    world.pending.push(Record::Approved {
                        call: *call,
                        by: Approver::Policy,
                    });
                }
            }
        }
        world.staged = decided
            .effects
            .into_iter()
            .map(|work| match work {
                Work::CallModel { id, .. } => Staged::Start(id, Job::Model { batches: 0 }),
                Work::RunTool {
                    id, tool, effect, ..
                } => Staged::Start(effect, Job::Tool { call: id, tool }),
                Work::Sleep { id, .. } => Staged::Start(id, Job::Sleep),
                Work::CancelModel { id } | Work::CancelTool { id } => Staged::Cancel(id),
                Work::AskApproval { id, .. } => Staged::Ask(id),
            })
            .collect();
        if world.pending.is_empty() {
            // Nothing to persist: the shell starts the effects at once.
            Self::sync(world);
        }
    }

    /// The one state the world boots into: persisting `resume()` of the
    /// empty log.
    pub(crate) fn boot(&self) -> World {
        let mut world = World::default();
        let boot = Core::<Abstract>::new(self.limits.clone()).resume();
        self.stage(&mut world, &boot);
        world
    }

    /// `world` with the shell's resume staged without a crash: the boot's
    /// resume was refused by a full log, and is decided again once the
    /// running turn is interrupted. `None` if the log does not recover.
    pub(crate) fn resumed(&self, world: &World) -> Option<World> {
        let mut next = world.clone();
        next.refused = false;
        self.stage(&mut next, &self.core(world)?.resume());
        Some(next)
    }

    /// The decision being persisted is durable: its records join the log
    /// and its effects start.
    fn sync(world: &mut World) {
        let mut pending = std::mem::take(&mut world.pending);
        world.log.append(&mut pending);
        for staged in std::mem::take(&mut world.staged) {
            match staged {
                Staged::Start(id, job) => {
                    if let Job::Tool { call, tool } = &job {
                        let entry = world.starts.entry(call.clone()).or_insert((*tool, 0));
                        entry.1 = entry.1.saturating_add(1);
                    }
                    let executed = !matches!(job, Job::Tool { .. });
                    world.flights.insert(
                        id,
                        Flight {
                            job,
                            executed,
                            cancelled: false,
                        },
                    );
                }
                Staged::Cancel(id) => {
                    if let Some(flight) = world.flights.get_mut(&id) {
                        flight.cancelled = true;
                    }
                }
                Staged::Ask(call) => {
                    world.asks.insert(call);
                }
            }
        }
    }

    /// Delivers `event` to the core and stages its decision; nothing, on a
    /// log that does not recover (every durable prefix recovers is a
    /// property).
    fn deliver(&self, world: &mut World, event: Event<Abstract>) {
        let Some(core) = self.core(world) else {
            return;
        };
        let completion = matches!(
            event,
            Event::ModelDone { .. } | Event::ToolDone { .. } | Event::Woke { .. }
        );
        if completion {
            world.last = Some(event.clone());
        }
        self.stage(world, &core.decide(event));
    }

    /// Delivers a reply that matches nothing live; it must change nothing.
    fn deliver_stale(&self, world: &mut World, event: Event<Abstract>) {
        let Some(core) = self.core(world) else {
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
    fn run(world: &mut World, job: &Job) {
        if let Job::Tool { call, tool } = job {
            let entry = world.runs.entry(call.clone()).or_insert((*tool, 0));
            entry.1 = entry.1.saturating_add(1);
        }
    }

    const fn reply(id: EffectId, flight: &Flight) -> Event<Abstract> {
        match &flight.job {
            Job::Model { .. } => Event::ModelDone {
                id,
                result: ModelExit::Finished(Vec::new()),
            },
            Job::Tool { .. } => Event::ToolDone {
                id,
                exit: Exit::Ok,
                output: size(1),
            },
            Job::Sleep => Event::Woke { id },
        }
    }

    /// What the stream serves a reader: the durable log (and, under the
    /// `ServePending` bug, the decision still being persisted).
    fn served(&self, world: &World) -> Vec<Rec> {
        let mut served = world.log.clone();
        if self.bug == Some(Bug::ServePending) {
            served.extend(world.pending.iter().cloned());
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
    fn executions(&self, state: &World, actions: &mut Vec<Action>) {
        for (id, flight) in &state.flights {
            if matches!(flight.job, Job::Tool { .. }) && !flight.executed {
                actions.push(Action::Execute(*id));
            }
        }
        if let Domain::Bounded(_) = self.domain {
            for (id, flight) in &state.orphans {
                if !flight.executed {
                    actions.push(Action::OrphanExecute(*id));
                }
            }
        }
    }

    /// The user's commands, delivered when the shell takes input.
    fn user_actions(&self, state: &World, actions: &mut Vec<Action>) {
        match &self.domain {
            Domain::Bounded(bounded) => {
                if usize::from(state.sent) <= bounded.messages {
                    for slot in 0..bounded.messages {
                        let (message, turn) = message_ids(slot);
                        actions.push(Action::Deliver(Event::Message {
                            message,
                            turn,
                            text: size(1),
                        }));
                    }
                }
                let answers = |call: &CallId, actions: &mut Vec<Action>| {
                    for verdict in [Verdict::Approve, Verdict::Deny] {
                        actions.push(Action::Deliver(Event::Approve {
                            call: call.clone(),
                            verdict,
                        }));
                    }
                };
                for call in &state.asks {
                    answers(call, actions);
                }
                if state.stale_answers < bounded.stale_answers {
                    for call in self.audit(state).proposed {
                        if !state.asks.contains(&call) {
                            answers(&call, actions);
                        }
                    }
                }
                if state.interrupts < bounded.max_interrupts
                    && let Some(turn) = self.core(state).and_then(|core| core.running())
                {
                    actions.push(Action::Deliver(Event::Interrupt { turn }));
                }
            }
            Domain::Trace(events) => actions.extend(
                events
                    .iter()
                    .filter(|event| {
                        matches!(
                            event,
                            Event::Message { .. } | Event::Approve { .. } | Event::Interrupt { .. }
                        )
                    })
                    .cloned()
                    .map(Action::Deliver),
            ),
        }
    }

    /// The replies effect `id`, in flight, may make.
    fn reply_actions(&self, id: EffectId, flight: &Flight, actions: &mut Vec<Action>) {
        let admitted = |event: &Event<Abstract>| admits(id, flight, event);
        match &self.domain {
            Domain::Bounded(bounded) => actions.extend(
                replies(bounded, id, &flight.job)
                    .into_iter()
                    .filter(admitted)
                    .map(Action::Deliver),
            ),
            Domain::Trace(events) => actions.extend(
                events
                    .iter()
                    .filter(|event| admitted(event))
                    .cloned()
                    .map(Action::Deliver),
            ),
        }
    }
}

impl Model for Harness {
    type State = World;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![self.boot()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.persisting() {
            // The shell persists the decision before it delivers anything
            // more to the core; meanwhile tools run and clients read.
            actions.push(Action::Sync);
            if self.may_crash(state) {
                for keep in 0..=state.pending.len() {
                    actions.push(Action::Crash(keep));
                }
            }
            self.executions(state, actions);
            // The stream may be read meanwhile: only durable records show.
            self.client_actions(state, actions);
            return;
        }
        self.user_actions(state, actions);
        self.executions(state, actions);
        for (id, flight) in &state.flights {
            self.reply_actions(*id, flight, actions);
        }
        if let Domain::Bounded(bounded) = &self.domain {
            for id in state.orphans.keys() {
                actions.push(Action::OrphanComplete(*id));
            }
            if state.duplicates < bounded.max_duplicates && state.last.is_some() {
                actions.push(Action::Duplicate);
            }
        }
        if self.may_crash(state) {
            actions.push(Action::Crash(0));
        }
        self.client_actions(state, actions);
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut world = last_state.clone();
        match action {
            Action::Sync => Self::sync(&mut world),
            Action::Crash(keep) => {
                world.crashes = world.crashes.saturating_add(1);
                // The client's connection dies with the daemon.
                world.client.cursor = None;
                let survived = world.pending.get(..keep)?.to_vec();
                world.log.extend(survived);
                world.pending.clear();
                world.staged.clear();
                world.asks.clear();
                world.refused = false;
                // Only tools outlive the process (a child may still run);
                // model streams and timers die with it.
                let flights = std::mem::take(&mut world.flights);
                world.orphans.extend(
                    flights
                        .into_iter()
                        .filter(|(_, flight)| matches!(flight.job, Job::Tool { .. })),
                );
                let Some(core) = self.core(&world) else {
                    return Some(world);
                };
                let mut resumed = core.resume();
                if self.bug == Some(Bug::LeaveDispatched)
                    && let Some(at) = resumed.records.iter().position(|record| {
                        matches!(
                            record,
                            Record::ToolResult {
                                exit: Exit::Unknown,
                                ..
                            }
                        )
                    })
                {
                    resumed.records.truncate(at);
                    resumed.effects.clear();
                }
                self.stage(&mut world, &resumed);
                if self.bug == Some(Bug::UnsafeResolvedNotStarted) {
                    for record in &mut world.pending {
                        if let Record::ToolResult { exit, .. } = record
                            && *exit == Exit::Unknown
                        {
                            *exit = Exit::NotStarted(Unstarted::Cancelled);
                        }
                    }
                }
                if self.bug == Some(Bug::RerunUnsafe) {
                    let epoch = core.epoch().0.saturating_add(1);
                    for (id, flight) in &world.orphans {
                        if let Job::Tool { tool, .. } = flight.job
                            && tool.rerun() == Rerun::Unsafe
                        {
                            world.staged.push(Staged::Start(
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
            Action::Deliver(event) => {
                match &event {
                    Event::Message { .. } => world.sent = world.sent.saturating_add(1),
                    Event::Approve { call, .. } => {
                        if !world.asks.remove(call) {
                            world.stale_answers = world.stale_answers.saturating_add(1);
                        }
                    }
                    Event::Interrupt { .. } => {
                        world.interrupts = world.interrupts.saturating_add(1);
                    }
                    Event::ModelParts { id, .. } => {
                        let Job::Model { batches } = &mut world.flights.get_mut(id)?.job else {
                            return None;
                        };
                        *batches = batches.saturating_add(1);
                    }
                    Event::ModelDone { id, .. }
                    | Event::ToolDone { id, .. }
                    | Event::Woke { id } => {
                        world.flights.remove(id)?;
                    }
                }
                self.deliver(&mut world, event);
            }
            Action::Execute(id) => {
                let flight = world.flights.get_mut(&id)?;
                flight.executed = true;
                let job = flight.job.clone();
                Self::run(&mut world, &job);
            }
            Action::OrphanExecute(id) => {
                let flight = world.orphans.get_mut(&id)?;
                flight.executed = true;
                let job = flight.job.clone();
                Self::run(&mut world, &job);
            }
            Action::OrphanComplete(id) => {
                let flight = world.orphans.remove(&id)?;
                self.deliver_stale(&mut world, Self::reply(id, &flight));
            }
            Action::Duplicate => {
                world.duplicates = world.duplicates.saturating_add(1);
                let event = world.last.clone()?;
                self.deliver_stale(&mut world, event);
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

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // U3. Runs are no more than starts: a run acts only in a flight.
            Property::always(
                "an Unsafe call is dispatched at most once across crashes",
                |_, world: &World| {
                    world
                        .starts
                        .values()
                        .all(|(tool, count)| tool.rerun() == Rerun::Safe || *count <= 1)
                },
            ),
            // U4.
            Property::always(
                "after a crash a dispatched Unsafe call resolves Unknown",
                |model: &Self, world: &World| {
                    !model
                        .audit(world)
                        .breaches
                        .contains(&Breach::UnsafeKnownAfterCrash)
                },
            ),
            // U5. Every log shape the core refuses (`spec_apply`) is caught
            // here.
            Property::always(
                "every durable prefix recovers",
                |model: &Self, world: &World| model.core(world).is_some(),
            ),
            // U6.
            Property::always(
                "the client sees a prefix of the durable log",
                |_, world: &World| {
                    world.client.next == world.client.seen.len()
                        && world.log.get(..world.client.seen.len())
                            == Some(world.client.seen.as_slice())
                },
            ),
            Property::always("stale completions change nothing", |_, world: &World| {
                !world.stale_changed
            }),
            Property::always(
                "calls past a bound never run",
                |model: &Self, world: &World| {
                    !model
                        .audit(world)
                        .breaches
                        .contains(&Breach::OverBoundDispatched)
                },
            ),
            Property::always(
                "quiescent implies the turn ended",
                |model: &Self, world: &World| {
                    !world.quiescent()
                        || model
                            .core(world)
                            .is_some_and(|core| core.running().is_none())
                },
            ),
            Property::sometimes("a tool runs", |_, world: &World| !world.runs.is_empty()),
            Property::sometimes("a turn completes", |_, world: &World| {
                world.log.iter().any(|record| {
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
                world.log.iter().any(|record| {
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
                world.log.iter().any(|record| {
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
                world.log.iter().any(|record| {
                    matches!(
                        record,
                        Record::StepReset {
                            reason: Reset::Retry { .. },
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
                |model: &Self, world: &World| model.audit(world).cancelled_after_finish.is_some(),
            ),
            Property::sometimes("a call fails by a limit", |model: &Self, world: &World| {
                model.audit(world).limited > 0
            }),
            Property::sometimes("two Unsafe calls run at once", |_, world: &World| {
                world
                    .flights
                    .values()
                    .filter(|flight| {
                        flight.executed
                            && matches!(
                                flight.job,
                                Job::Tool { tool, .. } if tool.rerun() == Rerun::Unsafe
                            )
                    })
                    .count()
                    >= 2
            }),
            Property::sometimes("a Safe call reruns after a crash", |_, world: &World| {
                world
                    .starts
                    .values()
                    .any(|(tool, count)| tool.rerun() == Rerun::Safe && *count >= 2)
            }),
            Property::sometimes("a model call streams a second batch", |_, world: &World| {
                world
                    .flights
                    .values()
                    .any(|flight| matches!(flight.job, Job::Model { batches } if batches >= 2))
            }),
            Property::sometimes("the client catches up", |_, world: &World| {
                world.quiescent()
                    && world.client.seen.len() == world.log.len()
                    && world
                        .log
                        .iter()
                        .any(|record| matches!(record, Record::TurnEnded { .. }))
            }),
            Property::sometimes(
                "a client resumed mid-stream catches up",
                |_, world: &World| {
                    world.quiescent()
                        && world.client.resumed_at.is_some_and(|from| {
                            world
                                .log
                                .iter()
                                .take(from)
                                .any(|record| matches!(record, Record::TurnStarted { .. }))
                        })
                        && world.client.seen.len() == world.log.len()
                },
            ),
        ]
    }
}
