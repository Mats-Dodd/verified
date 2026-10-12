//! Stateright model of the core inside an abstract world.
//!
//! The world is the durable log, the decision being persisted (which a crash
//! may tear to any prefix), effects in flight with an Execute/Complete split
//! for tools, effects orphaned by a crash (which may still execute and
//! reply), and facts about the outside: how often each call ran and which
//! approvals the user was asked for. The core is always `recover(log)`: it
//! has no other state. A client follows the stream: it reads committed
//! records from its offset, may reconnect, and must only ever see a prefix
//! of the durable log (the server projects records to chunks one record per
//! offset, so the projection is modelled as record-seq coverage). Checked by
//! BFS with small bounds.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::collections::{BTreeMap, BTreeSet};

use stateright::{Checker as _, Model, Property};
use verified_core::{Core, Decision, Effect, Event, ModelExit, Outcome, Verdict};
use verified_domain::{
    Abstract, Approver, CallId, CallIx, Cause, EffectId, Epoch, Exit, Fault, Limit, Limits,
    LimitsConfig, Measured, MessageId, Millis, Proposal, Record, Request, Rerun, Reset, Tool,
    TurnId, TurnOutcome, Unit, UnixMillis,
};

type Rec = Record<Abstract>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Job {
    Model { parts_sent: bool },
    Tool { call: CallId, tool: Tool },
    Sleep,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Flight {
    job: Job,
    /// The side effect happened (tools only; others start executed).
    executed: bool,
    cancelled: bool,
}

/// An effect of the decision being persisted; it starts at `Sync`.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Staged {
    Start(EffectId, Job),
    Cancel(EffectId),
    Ask(CallId),
}

/// A client following the stream: the records it has seen, from offset 0,
/// and the offset its next read starts at. Its connection has a cursor on
/// the server's side; a reconnect (or the daemon crashing) drops it, and the
/// client resumes on a new connection from its own offset.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Default)]
struct Client {
    seen: Vec<Rec>,
    next: usize,
    /// The server's cursor for the live connection; `None` while it is down.
    cursor: Option<usize>,
    reconnects: u8,
    /// The offset of the last resume, if one started past the beginning.
    resumed_at: Option<usize>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct World {
    log: Vec<Rec>,
    pending: Vec<Rec>,
    staged: Vec<Staged>,
    flights: BTreeMap<EffectId, Flight>,
    orphans: BTreeMap<EffectId, Flight>,
    asks: BTreeSet<CallId>,
    /// Times each call's side effect happened, with its tool.
    runs: BTreeMap<CallId, (Tool, u8)>,
    /// Times a run of each call was started (a `RunTool` reached the world),
    /// with its tool.
    starts: BTreeMap<CallId, (Tool, u8)>,
    sent: u8,
    crashes: u8,
    interrupts: u8,
    duplicates: u8,
    /// The last completion delivered, for late-duplicate redelivery.
    last: Option<Event<Abstract>>,
    client: Client,
    broken: Broken,
    stale_refused: bool,
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
            last: None,
            client: Client {
                cursor: Some(0),
                ..Client::default()
            },
            broken: Broken::default(),
            stale_refused: bool::default(),
        }
    }
}

/// Safety violations seen while stepping the world.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Default)]
struct Broken {
    unapproved_run: bool,
    stale_changed: bool,
    unrecoverable: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Action {
    Send(usize),
    Answer(CallId, Verdict),
    Interrupt,
    Execute(EffectId),
    Parts(EffectId),
    Finish(EffectId, usize),
    Fail(EffectId, Cause),
    ToolDone(EffectId, Exit),
    Woke(EffectId),
    OrphanExecute(EffectId),
    OrphanComplete(EffectId),
    Duplicate,
    Sync,
    Crash(usize),
    /// The client reads the next committed record on its connection.
    Read,
    /// The client opens a new connection, resuming from its offset.
    Reconnect,
}

/// A deliberately broken shell, to show the properties can fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bug {
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
}

#[derive(Clone, Debug)]
struct Harness {
    limits: Limits,
    /// Distinct messages the user may send; one extra send repeats the first.
    messages: usize,
    scripts: Vec<Vec<Tool>>,
    max_crashes: u8,
    max_interrupts: u8,
    max_duplicates: u8,
    /// Whether a client follows the stream (and may reconnect this often).
    client: Option<u8>,
    bug: Option<Bug>,
}

fn config(steps: u32) -> LimitsConfig {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    LimitsConfig {
        turns_per_session: count(4),
        steps_per_turn: NonZeroU32::new(steps).expect("at least one step"),
        calls_per_step: count(4),
        calls_per_turn: count(8),
        context_bytes: NonZeroU64::new(1_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    }
}

fn limits(steps: u32) -> Limits {
    Limits::parse(config(steps)).expect("consistent limits")
}

const fn payload() -> Measured<Unit> {
    Measured {
        body: Unit,
        bytes: 1,
    }
}

fn message_ids(slot: usize) -> (MessageId, TurnId) {
    let number = u64::try_from(slot).expect("few slots");
    let at = UnixMillis::new(1_700_000_000_000u64.saturating_add(number)).expect("a 48-bit time");
    (
        MessageId::from_parts(at, &[1; 10]),
        TurnId::from_parts(at, &[2; 10]),
    )
}

/// The calls of model response `script` to model call `id`; ids are unique
/// per model call, as the provider's are.
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

/// Facts read back from the log.
#[derive(Default)]
struct Audit {
    /// Each approved call, and who approved it.
    approved: BTreeMap<CallId, Approver>,
    dispatched: BTreeSet<CallId>,
    results: BTreeMap<CallId, u8>,
    proposed: BTreeSet<CallId>,
    turn_ends: BTreeMap<TurnId, u8>,
    breaches: BTreeSet<Breach>,
    /// Calls that failed by a limit.
    limited: u8,
    /// A turn was cancelled after its model call finished.
    cancelled_after_finish: Option<TurnId>,
}

/// A rule the log broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Breach {
    DispatchBeforeApproval,
    /// A turn the user stopped ended other than `Cancelled`.
    CancelLost,
    /// A call past `calls_per_step` or `calls_per_turn` was dispatched.
    OverBoundDispatched,
    /// Policy approved a call that only the user may approve.
    PolicyApprovedMutation,
    /// An `Unsafe` call dispatched before a crash was resolved after it
    /// other than `Unknown`.
    UnsafeKnownAfterCrash,
}

fn audit(log: &[Rec], limits: &Limits) -> Audit {
    let mut audit = Audit::default();
    let mut names: Vec<CallId> = Vec::new();
    let mut tools: Vec<Tool> = Vec::new();
    let mut over: BTreeSet<usize> = BTreeSet::new();
    // Unsafe calls dispatched and unresolved, and those of them a crash
    // (an `EpochBumped`) came between dispatch and result for.
    let mut unsafe_live: BTreeSet<usize> = BTreeSet::new();
    let mut crossed: BTreeSet<usize> = BTreeSet::new();
    let mut stopping: BTreeSet<TurnId> = BTreeSet::new();
    for record in log {
        match record {
            Record::TurnStarted { .. } => {
                names.clear();
                tools.clear();
                over.clear();
                unsafe_live.clear();
                crossed.clear();
            }
            Record::StepFinished { calls, step } => {
                if stopping.contains(&step.turn) {
                    audit.cancelled_after_finish = Some(step.turn);
                }
                let first = names.len();
                for proposal in calls {
                    let index = names.len();
                    if index.saturating_sub(first) >= limits.calls_per_step()
                        || index >= limits.calls_per_turn()
                    {
                        over.insert(index);
                    }
                    names.push(proposal.id.clone());
                    tools.push(match &proposal.request {
                        Request::Run { args } => *args,
                        Request::Invalid { .. } => Tool::Read,
                    });
                    audit.proposed.insert(proposal.id.clone());
                }
            }
            Record::CancelRequested { turn } => {
                stopping.insert(*turn);
            }
            Record::Approved {
                call: CallIx(index),
                by,
            } => {
                if let Some(name) = names.get(*index) {
                    audit.approved.insert(name.clone(), *by);
                }
                if *by == Approver::Policy
                    && tools.get(*index).map(|tool| tool.rerun()) != Some(Rerun::Safe)
                {
                    audit.breaches.insert(Breach::PolicyApprovedMutation);
                }
            }
            Record::Dispatched {
                call: CallIx(index),
            } => {
                if let Some(name) = names.get(*index) {
                    if !audit.approved.contains_key(name) {
                        audit.breaches.insert(Breach::DispatchBeforeApproval);
                    }
                    audit.dispatched.insert(name.clone());
                }
                if over.contains(index) {
                    audit.breaches.insert(Breach::OverBoundDispatched);
                }
                if tools.get(*index).map(|tool| tool.rerun()) == Some(Rerun::Unsafe) {
                    unsafe_live.insert(*index);
                }
            }
            Record::ToolResult {
                call: CallIx(index),
                exit,
                ..
            } => {
                if let Some(name) = names.get(*index) {
                    let count = audit.results.entry(name.clone()).or_default();
                    *count = count.saturating_add(1);
                }
                if *exit == Exit::Err(Cause::Failed(Fault::Limit(Limit::Calls))) {
                    audit.limited = audit.limited.saturating_add(1);
                }
                unsafe_live.remove(index);
                if crossed.remove(index) && *exit != Exit::Err(Cause::Unknown) {
                    audit.breaches.insert(Breach::UnsafeKnownAfterCrash);
                }
            }
            Record::TurnEnded { turn, outcome } => {
                let count = audit.turn_ends.entry(*turn).or_default();
                *count = count.saturating_add(1);
                if stopping.contains(turn) && *outcome != TurnOutcome::Cancelled {
                    audit.breaches.insert(Breach::CancelLost);
                }
            }
            Record::EpochBumped { .. } => {
                crossed.extend(unsafe_live.iter().copied());
            }
            Record::StepStarted { .. } | Record::ModelOutput { .. } | Record::StepReset { .. } => {}
        }
    }
    audit
}

impl Harness {
    fn audit(&self, world: &World) -> Audit {
        audit(&world.log, &self.limits)
    }

    fn core(&self, world: &World) -> Option<Core<Abstract>> {
        Core::recover(self.limits.clone(), &world.log).ok()
    }

    fn stage(&self, world: &mut World, decision: Decision<Abstract>) {
        world.pending = decision.records;
        if self.bug == Some(Bug::PolicyApprovesAll) {
            // The shell approves what it should ask about, as if by policy.
            for effect in &decision.effects {
                if let Effect::AskApproval { request } = effect {
                    world.pending.push(Record::Approved {
                        call: request.call(),
                        by: Approver::Policy,
                    });
                }
            }
        }
        world.staged = decision
            .effects
            .into_iter()
            .map(|effect| match effect {
                Effect::CallModel { id, .. } => Staged::Start(id, Job::Model { parts_sent: false }),
                Effect::RunTool { call } => Staged::Start(
                    call.effect(),
                    Job::Tool {
                        call: call.id().clone(),
                        tool: call.tool(),
                    },
                ),
                Effect::Sleep { id, .. } => Staged::Start(id, Job::Sleep),
                Effect::CancelModel { id } | Effect::CancelTool { id } => Staged::Cancel(id),
                Effect::AskApproval { request } => Staged::Ask(request.id().clone()),
            })
            .collect();
    }

    fn deliver(&self, world: &mut World, event: Event<Abstract>) {
        let Some(core) = self.core(world) else {
            world.broken.unrecoverable = true;
            return;
        };
        let completion = matches!(
            event,
            Event::ModelDone { .. } | Event::ToolDone { .. } | Event::Woke { .. }
        );
        if completion {
            world.last = Some(event.clone());
        }
        self.stage(world, core.decide(event));
    }

    /// Delivers a reply that matches nothing live; it must change nothing.
    fn deliver_stale(&self, world: &mut World, event: Event<Abstract>) {
        let Some(core) = self.core(world) else {
            world.broken.unrecoverable = true;
            return;
        };
        let decision = core.decide(event);
        if decision.outcome == Outcome::Stale
            && decision.records.is_empty()
            && decision.effects.is_empty()
        {
            world.stale_refused = true;
        } else {
            world.broken.stale_changed = true;
        }
    }

    /// A tool's side effect happens in the world: only after a durable
    /// approval by someone allowed to give it (policy for `Safe` tools, the
    /// user for the rest) and a durable dispatch.
    fn run(&self, world: &mut World, job: &Job) {
        if let Job::Tool { call, tool } = job {
            let facts = self.audit(world);
            let approved = match facts.approved.get(call) {
                Some(Approver::User) => true,
                Some(Approver::Policy) => tool.rerun() == Rerun::Safe,
                None => false,
            };
            if !(approved && facts.dispatched.contains(call)) {
                world.broken.unapproved_run = true;
            }
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
                output: payload(),
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
        let Some(reconnects) = self.client else {
            return;
        };
        if let Some(cursor) = state.client.cursor
            && self.served(state).len() > cursor
        {
            actions.push(Action::Read);
        }
        // A dropped connection is always reopened; a live one may also drop
        // (up to `reconnects` times).
        if state.client.cursor.is_none() || state.client.reconnects < reconnects {
            actions.push(Action::Reconnect);
        }
    }

    fn quiescent(world: &World) -> bool {
        world.pending.is_empty()
            && world.staged.is_empty()
            && world.flights.is_empty()
            && world.asks.is_empty()
    }
}

impl Model for Harness {
    type State = World;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![World::default()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if !state.pending.is_empty() || !state.staged.is_empty() {
            // The shell persists the decision before anything else happens.
            actions.push(Action::Sync);
            if state.crashes < self.max_crashes {
                for keep in 0..=state.pending.len() {
                    actions.push(Action::Crash(keep));
                }
            }
            // The stream may be read meanwhile: only durable records show.
            self.client_actions(state, actions);
            return;
        }
        if usize::from(state.sent) <= self.messages {
            for slot in 0..self.messages {
                actions.push(Action::Send(slot));
            }
        }
        for call in &state.asks {
            actions.push(Action::Answer(call.clone(), Verdict::Approve));
            actions.push(Action::Answer(call.clone(), Verdict::Deny));
        }
        if state.interrupts < self.max_interrupts
            && self.core(state).and_then(|core| core.running()).is_some()
        {
            actions.push(Action::Interrupt);
        }
        for (id, flight) in &state.flights {
            match &flight.job {
                Job::Model { parts_sent } => {
                    if !parts_sent {
                        actions.push(Action::Parts(*id));
                    }
                    for script in 0..self.scripts.len() {
                        actions.push(Action::Finish(*id, script));
                    }
                    actions.push(Action::Fail(*id, Cause::Failed(Fault::Overloaded)));
                    actions.push(Action::Fail(*id, Cause::Failed(Fault::Other)));
                    if flight.cancelled {
                        actions.push(Action::Fail(*id, Cause::Interrupted));
                    }
                }
                Job::Tool { .. } => {
                    if flight.executed {
                        actions.push(Action::ToolDone(*id, Exit::Ok));
                        actions.push(Action::ToolDone(
                            *id,
                            Exit::Err(Cause::Failed(Fault::Other)),
                        ));
                    } else {
                        actions.push(Action::Execute(*id));
                    }
                    if flight.cancelled {
                        actions.push(Action::ToolDone(*id, Exit::Err(Cause::Interrupted)));
                    }
                }
                Job::Sleep => actions.push(Action::Woke(*id)),
            }
        }
        for (id, flight) in &state.orphans {
            if !flight.executed {
                actions.push(Action::OrphanExecute(*id));
            }
            actions.push(Action::OrphanComplete(*id));
        }
        if state.duplicates < self.max_duplicates && state.last.is_some() {
            actions.push(Action::Duplicate);
        }
        if state.crashes < self.max_crashes {
            actions.push(Action::Crash(0));
        }
        self.client_actions(state, actions);
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut world = last_state.clone();
        match action {
            Action::Sync => {
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
            Action::Crash(keep) => {
                world.crashes = world.crashes.saturating_add(1);
                // The client's connection dies with the daemon.
                world.client.cursor = None;
                let survived = world.pending.get(..keep)?.to_vec();
                world.log.extend(survived);
                world.pending.clear();
                world.staged.clear();
                world.asks.clear();
                // Only tools outlive the process (a child may still run);
                // model streams and timers die with it.
                let flights = std::mem::take(&mut world.flights);
                world.orphans.extend(
                    flights
                        .into_iter()
                        .filter(|(_, flight)| matches!(flight.job, Job::Tool { .. })),
                );
                let Some(core) = self.core(&world) else {
                    world.broken.unrecoverable = true;
                    return Some(world);
                };
                self.stage(&mut world, core.resume());
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
            Action::Send(slot) => {
                world.sent = world.sent.saturating_add(1);
                let (message, turn) = message_ids(slot);
                self.deliver(
                    &mut world,
                    Event::Message {
                        message,
                        turn,
                        text: payload(),
                    },
                );
            }
            Action::Answer(call, verdict) => {
                world.asks.remove(&call);
                self.deliver(&mut world, Event::Approve { call, verdict });
            }
            Action::Interrupt => {
                world.interrupts = world.interrupts.saturating_add(1);
                let turn = self.core(&world)?.running()?;
                self.deliver(&mut world, Event::Interrupt { turn });
            }
            Action::Execute(id) => {
                let flight = world.flights.get_mut(&id)?;
                flight.executed = true;
                let job = flight.job.clone();
                self.run(&mut world, &job);
            }
            Action::Parts(id) => {
                if let Job::Model { parts_sent } = &mut world.flights.get_mut(&id)?.job {
                    *parts_sent = true;
                }
                self.deliver(
                    &mut world,
                    Event::ModelParts {
                        id,
                        parts: payload(),
                    },
                );
            }
            Action::Finish(id, script) => {
                world.flights.remove(&id)?;
                let calls = proposals(id, self.scripts.get(script)?);
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
            Action::ToolDone(id, exit) => {
                world.flights.remove(&id)?;
                self.deliver(
                    &mut world,
                    Event::ToolDone {
                        id,
                        exit,
                        output: payload(),
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
                self.run(&mut world, &job);
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
            Property::always(
                "tools run only after durable approval",
                |model: &Self, world: &World| {
                    let breaches = model.audit(world).breaches;
                    !world.broken.unapproved_run
                        && !breaches.contains(&Breach::DispatchBeforeApproval)
                        && !breaches.contains(&Breach::PolicyApprovedMutation)
                },
            ),
            Property::always(
                "an Unsafe call is dispatched at most once across crashes",
                |_, world: &World| {
                    world
                        .starts
                        .values()
                        .chain(world.runs.values())
                        .all(|(tool, count)| tool.rerun() == Rerun::Safe || *count <= 1)
                },
            ),
            Property::always(
                "after a crash a dispatched Unsafe call resolves Unknown and never reruns",
                |model: &Self, world: &World| {
                    !model
                        .audit(world)
                        .breaches
                        .contains(&Breach::UnsafeKnownAfterCrash)
                        && world
                            .starts
                            .values()
                            .all(|(tool, count)| tool.rerun() == Rerun::Safe || *count <= 1)
                },
            ),
            Property::always(
                "each call has at most one result",
                |model: &Self, world: &World| {
                    model.audit(world).results.values().all(|count| *count <= 1)
                },
            ),
            Property::always(
                "each turn ends at most once",
                |model: &Self, world: &World| {
                    model
                        .audit(world)
                        .turn_ends
                        .values()
                        .all(|count| *count <= 1)
                },
            ),
            Property::always(
                "an interrupted turn ends cancelled",
                |model: &Self, world: &World| {
                    !model.audit(world).breaches.contains(&Breach::CancelLost)
                },
            ),
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
                "the client sees a prefix of the durable log",
                |_, world: &World| {
                    world.client.next == world.client.seen.len()
                        && world.log.get(..world.client.seen.len()) == Some(&world.client.seen[..])
                },
            ),
            Property::always("stale completions change nothing", |_, world: &World| {
                !world.broken.stale_changed
            }),
            Property::always("every durable prefix recovers", |_, world: &World| {
                !world.broken.unrecoverable
            }),
            Property::always(
                "quiescent implies every call resolved and the turn ended",
                |model: &Self, world: &World| {
                    if !Self::quiescent(world) {
                        return true;
                    }
                    let facts = model.audit(world);
                    let resolved = facts
                        .proposed
                        .iter()
                        .all(|call| facts.results.contains_key(call));
                    let idle = model
                        .core(world)
                        .is_some_and(|core| core.running().is_none());
                    resolved && idle
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
            Property::sometimes("the client catches up", |_, world: &World| {
                Self::quiescent(world)
                    && !world.log.is_empty()
                    && world.client.seen.len() == world.log.len()
            }),
            Property::sometimes(
                "a client resumed mid-stream catches up",
                |_, world: &World| {
                    Self::quiescent(world)
                        && world.client.resumed_at.is_some_and(|from| from > 0)
                        && world.client.seen.len() == world.log.len()
                },
            ),
        ]
    }
}

/// Every fault except crashes: interrupts and late duplicates.
fn without_crashes() -> Harness {
    Harness {
        limits: limits(2),
        messages: 1,
        scripts: vec![Vec::new(), vec![Tool::Read], vec![Tool::Bash]],
        max_crashes: 0,
        max_interrupts: 1,
        max_duplicates: 1,
        client: None,
        bug: None,
    }
}

/// A crash anywhere, including mid-persist, with tools of every class.
fn with_crashes(scripts: Vec<Vec<Tool>>, steps: u32) -> Harness {
    Harness {
        limits: limits(steps),
        messages: 1,
        scripts,
        max_crashes: 1,
        max_interrupts: 0,
        max_duplicates: 0,
        client: None,
        bug: None,
    }
}

const SAFETY: [&str; 11] = [
    "tools run only after durable approval",
    "an Unsafe call is dispatched at most once across crashes",
    "after a crash a dispatched Unsafe call resolves Unknown and never reruns",
    "each call has at most one result",
    "each turn ends at most once",
    "stale completions change nothing",
    "every durable prefix recovers",
    "quiescent implies every call resolved and the turn ended",
    "an interrupted turn ends cancelled",
    "calls past a bound never run",
    "the client sees a prefix of the durable log",
];

/// Checks every safety property, and that each `reachable` example occurs
/// (so the safety checks are not vacuous). Returns the states explored.
fn check(harness: Harness, reachable: &[&'static str]) -> usize {
    let checker = harness.checker().spawn_bfs().join();
    for name in SAFETY {
        let counterexample = checker.discovery(name).map(stateright::Path::into_actions);
        assert_eq!(counterexample, None, "{name} is violated");
    }
    for name in reachable {
        assert!(checker.discovery(name).is_some(), "never reached: {name}");
    }
    checker.unique_state_count()
}

#[test]
fn interrupts_and_duplicates() {
    let states = check(
        without_crashes(),
        &[
            "a tool runs",
            "a turn completes",
            "a turn is cancelled",
            "a step is retried",
            "a stale completion is refused",
            "a turn is cancelled after its model finished",
        ],
    );
    assert!(states > 10_000, "a real state space: {states}");
}

#[test]
fn crashes_with_every_tool_class() {
    let states = check(
        with_crashes(
            vec![Vec::new(), vec![Tool::Bash], vec![Tool::Edit, Tool::Read]],
            1,
        ),
        &[
            "a tool runs",
            "a turn completes",
            "an outcome is unknown",
            "a stale completion is refused",
        ],
    );
    assert!(states > 10_000, "a real state space: {states}");
}

#[test]
fn crashes_across_steps() {
    let states = check(
        with_crashes(vec![Vec::new(), vec![Tool::Edit]], 2),
        &[
            "a tool runs",
            "a turn completes",
            "a stale completion is refused",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn crashes_while_cancelling() {
    let states = check(
        Harness {
            max_interrupts: 1,
            ..with_crashes(vec![Vec::new(), vec![Tool::Bash], vec![Tool::Edit]], 1)
        },
        &["a turn is cancelled", "an outcome is unknown"],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn rerunning_an_unsafe_call_after_a_crash_is_caught() {
    for tool in [Tool::Edit, Tool::Bash] {
        let checker = Harness {
            bug: Some(Bug::RerunUnsafe),
            ..with_crashes(vec![vec![tool]], 1)
        }
        .checker()
        .spawn_bfs()
        .join();
        for name in [
            "an Unsafe call is dispatched at most once across crashes",
            "after a crash a dispatched Unsafe call resolves Unknown and never reruns",
        ] {
            assert!(
                checker.discovery(name).is_some(),
                "the checker finds the second {tool:?} run: {name}"
            );
        }
    }
}

#[test]
fn resolving_an_unsafe_call_known_after_a_crash_is_caught() {
    for tool in [Tool::Edit, Tool::Bash] {
        let checker = Harness {
            bug: Some(Bug::UnsafeResolvedInterrupted),
            ..with_crashes(vec![vec![tool]], 1)
        }
        .checker()
        .spawn_bfs()
        .join();
        assert!(
            checker
                .discovery(
                    "after a crash a dispatched Unsafe call resolves Unknown and never reruns"
                )
                .is_some(),
            "the checker sees the {tool:?} call resolved as if it never ran"
        );
        for name in [
            "an Unsafe call is dispatched at most once across crashes",
            "every durable prefix recovers",
        ] {
            assert_eq!(
                checker.discovery(name).map(stateright::Path::into_actions),
                None,
                "only the Unknown audit fires for {tool:?}: {name}"
            );
        }
    }
}

#[test]
fn approval_by_policy_of_an_edit_is_caught() {
    let checker = Harness {
        bug: Some(Bug::PolicyApprovesAll),
        ..with_crashes(vec![vec![Tool::Edit]], 1)
    }
    .checker()
    .spawn_bfs()
    .join();
    assert!(
        checker
            .discovery("tools run only after durable approval")
            .is_some(),
        "the checker sees who approved the edit"
    );
}

#[test]
fn calls_past_the_bounds() {
    let states = check(
        Harness {
            limits: Limits::parse(LimitsConfig {
                calls_per_step: NonZeroUsize::new(1).expect("non-zero"),
                calls_per_turn: NonZeroUsize::new(2).expect("non-zero"),
                ..config(3)
            })
            .expect("consistent limits"),
            ..with_crashes(vec![Vec::new(), vec![Tool::Read, Tool::Edit]], 3)
        },
        &["a tool runs", "a turn completes", "a call fails by a limit"],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn unsafe_calls_run_together() {
    let states = check(
        with_crashes(vec![vec![Tool::Edit, Tool::Bash, Tool::Read]], 1),
        &[
            "two Unsafe calls run at once",
            "a Safe call reruns after a crash",
            "an outcome is unknown",
            "a stale completion is refused",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn a_client_follows_the_stream() {
    let states = check(
        Harness {
            client: Some(1),
            ..with_crashes(vec![Vec::new(), vec![Tool::Read]], 1)
        },
        &[
            "the client catches up",
            "a client resumed mid-stream catches up",
            "a turn completes",
        ],
    );
    assert!(states > 1_000, "a real state space: {states}");
}

#[test]
fn serving_records_before_they_are_durable_is_caught() {
    let checker = Harness {
        client: Some(0),
        bug: Some(Bug::ServePending),
        ..with_crashes(vec![Vec::new()], 1)
    }
    .checker()
    .spawn_bfs()
    .join();
    assert!(
        checker
            .discovery("the client sees a prefix of the durable log")
            .is_some(),
        "a crash tears records the client already saw"
    );
}

#[test]
fn skipping_a_record_on_reconnect_is_caught() {
    let checker = Harness {
        client: Some(1),
        bug: Some(Bug::SkipOnReconnect),
        ..with_crashes(vec![Vec::new()], 1)
    }
    .checker()
    .spawn_bfs()
    .join();
    assert!(
        checker
            .discovery("the client sees a prefix of the durable log")
            .is_some(),
        "the resumed reader misses a record"
    );
}

#[test]
#[ignore = "larger bounds: minutes of BFS; run with --run-ignored only"]
fn everything_at_once() {
    check(
        Harness {
            limits: limits(2),
            messages: 1,
            scripts: vec![Vec::new(), vec![Tool::Read], vec![Tool::Bash]],
            max_crashes: 1,
            max_interrupts: 1,
            max_duplicates: 1,
            client: Some(1),
            bug: None,
        },
        &[],
    );
}

#[test]
#[ignore = "larger bounds: hours of BFS; run with --run-ignored only"]
fn larger_bounds() {
    check(
        Harness {
            limits: limits(3),
            messages: 2,
            scripts: vec![
                Vec::new(),
                vec![Tool::Read],
                vec![Tool::Edit, Tool::Bash],
                vec![Tool::Bash, Tool::Read],
            ],
            max_crashes: 2,
            max_interrupts: 1,
            max_duplicates: 1,
            client: Some(1),
            bug: None,
        },
        &[],
    );
}
