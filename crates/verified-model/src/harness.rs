//! The model: the real core inside an abstract world, its actions, and the
//! properties every reachable state must keep.

use std::collections::BTreeMap;

use stateright::{Model, Property};
use verified_core::{Core, Decision, Event, ModelExit, Outcome, Verdict};
use verified_domain::{
    Abstract, Approver, CallId, Cause, EffectId, Epoch, Exit, Fault, Limits, Measured, MessageId,
    Proposal, Record, Request, Rerun, Reset, Tool, TurnId, TurnOutcome, Unit, UnixMillis,
};

use crate::alpha::{self, Work};
use crate::audit::{Audit, Breach, audit};
use crate::world::{Flight, Job, Rec, Staged, World};

/// A message the user sends: the ids the client and the shell made for it,
/// and its text's size.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Message {
    pub message: MessageId,
    pub turn: TurnId,
    pub bytes: u64,
}

/// One step of the world. Each action carries the values its event does
/// (ids, sizes, a model's proposals), so a trace's events are actions as
/// they are; which values are offered is the [`Domain`]'s.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    /// The user sends a message.
    Send(Message),
    /// The user answers an approval request.
    Answer(CallId, Verdict),
    /// The user asks to stop a turn.
    Interrupt(TurnId),
    /// A tool's side effect happens: the shell's `ToolRan` point, where its
    /// run has returned.
    Execute(EffectId),
    /// A model call streams a batch of parts of this size.
    Parts(EffectId, u64),
    /// A model call finishes, proposing these calls.
    Finish(EffectId, Vec<Proposal<Abstract>>),
    Fail(EffectId, Cause),
    /// A tool reports, with output of this size.
    ToolDone(EffectId, Exit, u64),
    Woke(EffectId),
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

/// The kind of an [`Action`], without its values.
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
            Self::Send(_) => Kind::Send,
            Self::Answer(..) => Kind::Answer,
            Self::Interrupt(_) => Kind::Interrupt,
            Self::Execute(_) => Kind::Execute,
            Self::Parts(..) => Kind::Parts,
            Self::Finish(..) => Kind::Finish,
            Self::Fail(..) => Kind::Fail,
            Self::ToolDone(..) => Kind::ToolDone,
            Self::Woke(_) => Kind::Woke,
            Self::OrphanExecute(_) => Kind::OrphanExecute,
            Self::OrphanComplete(_) => Kind::OrphanComplete,
            Self::Duplicate => Kind::Duplicate,
            Self::Sync => Kind::Sync,
            Self::Crash(_) => Kind::Crash,
            Self::Read => Kind::Read,
            Self::Reconnect => Kind::Reconnect,
        }
    }
}

/// The values actions may carry.
#[derive(Clone, Debug)]
pub enum Domain {
    /// A small synthetic set, for BFS.
    Bounded(Bounded),
    /// The values a trace carries, to replay it.
    Trace(Offers),
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

/// The values a trace carries, offered wherever the model allows them.
///
/// User commands are offered whenever the shell takes input (the user may
/// repeat or race them); completions only for their own effect. Crashes,
/// interrupts and reconnects are unbounded, and nothing only the model's
/// world does (late duplicates, orphans acting or replying) is offered.
#[derive(Clone, Debug, Default)]
pub struct Offers {
    pub messages: Vec<Message>,
    pub answers: Vec<(CallId, Verdict)>,
    pub interrupts: Vec<TurnId>,
    /// Batch sizes each model call streamed.
    pub parts: BTreeMap<EffectId, Vec<u64>>,
    pub responses: BTreeMap<EffectId, Vec<Vec<Proposal<Abstract>>>>,
    pub failures: BTreeMap<EffectId, Vec<Cause>>,
    /// Each tool run's exit and output size.
    pub reports: BTreeMap<EffectId, Vec<(Exit, u64)>>,
}

fn offer<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

impl Offers {
    /// Every value `actions` carry.
    #[must_use]
    pub fn of(actions: &[Action]) -> Self {
        let mut offers = Self::default();
        for action in actions {
            match action {
                Action::Send(message) => offer(&mut offers.messages, *message),
                Action::Answer(call, verdict) => {
                    offer(&mut offers.answers, (call.clone(), *verdict));
                }
                Action::Interrupt(turn) => offer(&mut offers.interrupts, *turn),
                Action::Parts(id, bytes) => offer(offers.parts.entry(*id).or_default(), *bytes),
                Action::Finish(id, calls) => {
                    offer(offers.responses.entry(*id).or_default(), calls.clone());
                }
                Action::Fail(id, cause) => offer(offers.failures.entry(*id).or_default(), *cause),
                Action::ToolDone(id, exit, bytes) => {
                    offer(offers.reports.entry(*id).or_default(), (*exit, *bytes));
                }
                Action::Execute(_)
                | Action::Woke(_)
                | Action::OrphanExecute(_)
                | Action::OrphanComplete(_)
                | Action::Duplicate
                | Action::Sync
                | Action::Crash(_)
                | Action::Read
                | Action::Reconnect => {}
            }
        }
        offers
    }
}

/// A deliberately broken shell, to show the properties can fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// After a crash, rerun dispatched `Unsafe` calls as if they were `Safe`.
    RerunUnsafe,
    /// After a crash, resolve dispatched `Unsafe` calls `Interrupted`, as
    /// if the shell knew they never ran.
    UnsafeResolvedInterrupted,
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

/// Whether a tool in `flight` may report `exit`: an interrupted run only
/// once it was asked to stop; anything else only once it ran.
const fn reportable(flight: &Flight, exit: Exit) -> bool {
    match exit {
        Exit::Err(Cause::Interrupted) => flight.cancelled,
        Exit::Ok | Exit::Err(Cause::Failed(_) | Cause::Unknown | Cause::Denied) => flight.executed,
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

    /// A harness for replaying traces run under `limits`: actions carry the
    /// values `offers` holds.
    #[must_use]
    pub const fn trace(limits: Limits, offers: Offers) -> Self {
        Self {
            limits,
            domain: Domain::Trace(offers),
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
                        actions.push(Action::Send(Message {
                            message,
                            turn,
                            bytes: 1,
                        }));
                    }
                }
                for call in &state.asks {
                    actions.push(Action::Answer(call.clone(), Verdict::Approve));
                    actions.push(Action::Answer(call.clone(), Verdict::Deny));
                }
                if state.stale_answers < bounded.stale_answers {
                    for call in self.audit(state).proposed {
                        if !state.asks.contains(&call) {
                            actions.push(Action::Answer(call.clone(), Verdict::Approve));
                            actions.push(Action::Answer(call, Verdict::Deny));
                        }
                    }
                }
                if state.interrupts < bounded.max_interrupts
                    && let Some(turn) = self.core(state).and_then(|core| core.running())
                {
                    actions.push(Action::Interrupt(turn));
                }
            }
            Domain::Trace(offers) => {
                actions.extend(offers.messages.iter().copied().map(Action::Send));
                actions.extend(
                    offers
                        .answers
                        .iter()
                        .map(|(call, verdict)| Action::Answer(call.clone(), *verdict)),
                );
                actions.extend(offers.interrupts.iter().copied().map(Action::Interrupt));
            }
        }
    }

    /// What model call `id`, having streamed `batches`, may do next.
    fn model_actions(&self, id: EffectId, batches: u8, cancelled: bool, actions: &mut Vec<Action>) {
        let failures = [
            Cause::Failed(Fault::Overloaded),
            Cause::Failed(Fault::Other),
            Cause::Interrupted,
        ];
        let (sizes, responses, failures): (Vec<u64>, Vec<Vec<Proposal<Abstract>>>, Vec<Cause>) =
            match &self.domain {
                Domain::Bounded(bounded) => (
                    if batches < bounded.batches {
                        vec![1]
                    } else {
                        Vec::new()
                    },
                    bounded
                        .scripts
                        .iter()
                        .map(|script| proposals(id, script))
                        .collect(),
                    failures.to_vec(),
                ),
                Domain::Trace(offers) => (
                    offers.parts.get(&id).cloned().unwrap_or_default(),
                    offers.responses.get(&id).cloned().unwrap_or_default(),
                    offers.failures.get(&id).cloned().unwrap_or_default(),
                ),
            };
        actions.extend(sizes.into_iter().map(|bytes| Action::Parts(id, bytes)));
        actions.extend(responses.into_iter().map(|calls| Action::Finish(id, calls)));
        actions.extend(
            failures
                .into_iter()
                .filter(|cause| *cause != Cause::Interrupted || cancelled)
                .map(|cause| Action::Fail(id, cause)),
        );
    }

    /// The reports tool run `id` may make.
    fn tool_actions(&self, id: EffectId, flight: &Flight, actions: &mut Vec<Action>) {
        let reports = match &self.domain {
            Domain::Bounded(_) => vec![
                (Exit::Ok, 1),
                (Exit::Err(Cause::Failed(Fault::Other)), 1),
                (Exit::Err(Cause::Interrupted), 1),
            ],
            Domain::Trace(offers) => offers.reports.get(&id).cloned().unwrap_or_default(),
        };
        actions.extend(
            reports
                .into_iter()
                .filter(|(exit, _)| reportable(flight, *exit))
                .map(|(exit, bytes)| Action::ToolDone(id, exit, bytes)),
        );
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
            match &flight.job {
                Job::Model { batches } => {
                    self.model_actions(*id, *batches, flight.cancelled, actions);
                }
                Job::Tool { .. } => self.tool_actions(*id, flight, actions),
                Job::Sleep => actions.push(Action::Woke(*id)),
            }
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
                                exit: Exit::Err(Cause::Unknown),
                                ..
                            }
                        )
                    })
                {
                    resumed.records.truncate(at);
                    resumed.effects.clear();
                }
                self.stage(&mut world, &resumed);
                if self.bug == Some(Bug::UnsafeResolvedInterrupted) {
                    for record in &mut world.pending {
                        if let Record::ToolResult { exit, .. } = record
                            && *exit == Exit::Err(Cause::Unknown)
                        {
                            *exit = Exit::Err(Cause::Interrupted);
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
            Action::Send(message) => {
                world.sent = world.sent.saturating_add(1);
                self.deliver(
                    &mut world,
                    Event::Message {
                        message: message.message,
                        turn: message.turn,
                        text: size(message.bytes),
                    },
                );
            }
            Action::Answer(call, verdict) => {
                if !world.asks.remove(&call) {
                    world.stale_answers = world.stale_answers.saturating_add(1);
                }
                self.deliver(&mut world, Event::Approve { call, verdict });
            }
            Action::Interrupt(turn) => {
                world.interrupts = world.interrupts.saturating_add(1);
                self.deliver(&mut world, Event::Interrupt { turn });
            }
            Action::Execute(id) => {
                let flight = world.flights.get_mut(&id)?;
                flight.executed = true;
                let job = flight.job.clone();
                Self::run(&mut world, &job);
            }
            Action::Parts(id, bytes) => {
                let Job::Model { batches } = &mut world.flights.get_mut(&id)?.job else {
                    return None;
                };
                *batches = batches.saturating_add(1);
                self.deliver(
                    &mut world,
                    Event::ModelParts {
                        id,
                        parts: size(bytes),
                    },
                );
            }
            Action::Finish(id, calls) => {
                world.flights.remove(&id)?;
                self.deliver(
                    &mut world,
                    Event::ModelDone {
                        id,
                        result: ModelExit::Finished(calls),
                    },
                );
            }
            Action::Fail(id, cause) => {
                world.flights.remove(&id)?;
                self.deliver(
                    &mut world,
                    Event::ModelDone {
                        id,
                        result: ModelExit::Failed(cause),
                    },
                );
            }
            Action::ToolDone(id, exit, bytes) => {
                world.flights.remove(&id)?;
                self.deliver(
                    &mut world,
                    Event::ToolDone {
                        id,
                        exit,
                        output: size(bytes),
                    },
                );
            }
            Action::Woke(id) => {
                world.flights.remove(&id)?;
                self.deliver(&mut world, Event::Woke { id });
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
            Property::sometimes("an outcome is unknown", |_, world: &World| {
                world.log.iter().any(|record| {
                    matches!(
                        record,
                        Record::ToolResult {
                            exit: Exit::Err(Cause::Unknown),
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
