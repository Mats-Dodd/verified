//! Behaviour of the core, driven the way the shell drives it: decide,
//! persist, apply, and on a crash recover from the log and resume.
//!
//! Scenario tests pin each rule; the proptests at the end check that decide
//! and apply never panic and that recovery reproduces the live state.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use proptest::prelude::*;
use verified_core::{
    Core, Corrupt, Decision, Effect, Event, ModelExit, Outcome, Rejection, Verdict, Violation,
};
use verified_domain::{
    Abstract, ActorId, ActorName, Address, Body, Born, CallId, CallIx, EffectId, Envelope,
    EnvelopeId, Epoch, Exit, Failure, Grant, Limit, Limits, LimitsConfig, Measured, MessageId,
    Millis, ModelFault, Payload, Proposal, Record, Request, Reset, Rules, Shape, StepId, TaskId,
    Tool, TurnId, TurnOutcome, Unit, UnixMillis, Unstarted,
};

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Bounds small enough to reach every limit in a test.
fn small_config() -> LimitsConfig {
    LimitsConfig {
        turns_per_actor: count(4),
        messages_per_actor: count(4),
        tasks_per_actor: count(2),
        depth: 1,
        steps_per_turn: NonZeroU32::new(4).expect("non-zero"),
        calls_per_step: count(4),
        calls_per_turn: count(8),
        context_bytes: NonZeroU64::new(1_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10), Millis::new(20)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    }
}

const fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("non-zero")
}

fn small_limits() -> Limits {
    Limits::new(small_config())
}

const fn payload(bytes: u64) -> Measured<Unit> {
    Measured { body: Unit, bytes }
}

/// A distinct instant per number, so ids minted from it differ.
fn at(number: u64) -> UnixMillis {
    UnixMillis::new(1_700_000_000_000u64.saturating_add(number)).expect("a 48-bit time")
}

/// The turn whose `TurnStarted` is record `seq`.
const fn turn(seq: u64) -> TurnId {
    TurnId(seq)
}

/// The first turn of a driver's log: its rules (0), birth (1), the first
/// message (2), then the turn (3).
const FIRST: TurnId = turn(3);

fn message(number: u64) -> MessageId {
    MessageId::from_parts(at(number), &[2; 10])
}

/// The actor every test drives.
fn actor() -> ActorId {
    ActorId::from_parts(at(0), &[4; 10])
}

fn born() -> Born {
    Born::root(
        actor(),
        ActorName::parse("main").expect("a name"),
        vec![Tool::Read, Tool::Edit, Tool::Bash],
    )
}

/// The user's message `number`, of `bytes`.
fn envelope(number: u64, bytes: u64) -> Envelope<Abstract> {
    Envelope {
        id: EnvelopeId::User(message(number)),
        from: Address::User,
        to: actor(),
        reply_to: None,
        body: Body::Text(payload(bytes)),
    }
}

fn deliver(number: u64, bytes: u64) -> Event<Abstract> {
    Event::Deliver {
        envelope: envelope(number, bytes),
    }
}

fn delivered(number: u64, bytes: u64) -> Record<Abstract> {
    Record::Delivered {
        envelope: envelope(number, bytes),
    }
}

const fn step(index: u32) -> StepId {
    StepId { turn: FIRST, index }
}

/// Step `index` of the first turn starts, carrying nothing.
const fn started(index: u32) -> Record<Abstract> {
    Record::StepStarted {
        step: step(index),
        carries: Vec::new(),
    }
}

/// The first turn's first step, carrying message 1.
fn first_step() -> Record<Abstract> {
    Record::StepStarted {
        step: step(0),
        carries: vec![EnvelopeId::User(message(1))],
    }
}

/// What a log starts with: the default rules, then the actor's birth.
fn boot() -> Vec<Record<Abstract>> {
    vec![
        Record::RulesLoaded {
            rules: Rules::default(),
            file: None,
        },
        Record::Born { born: born() },
    ]
}

fn call_id(name: &str) -> CallId {
    CallId::parse(name).expect("a protocol id")
}

fn propose(name: &str, tool: Tool) -> Proposal<Abstract> {
    Proposal {
        id: call_id(name),
        request: Request::Run {
            args: Shape::plain(tool, actor(), TaskId(0)),
        },
    }
}

/// The core plus its durable log, driven the way the shell drives it.
#[derive(Debug)]
struct Driver {
    limits: Limits,
    core: Core<Abstract>,
    log: Vec<Record<Abstract>>,
}

impl Driver {
    /// A core whose log holds the default rules (`read: allow`) and the
    /// actor's birth, as the shell's first boot records them.
    fn new(limits: Limits) -> Self {
        let mut driver = Self {
            core: Core::new(limits.clone()),
            limits,
            log: Vec::new(),
        };
        for record in boot() {
            driver.core.apply(&record).expect("the boot applies");
            driver.log.push(record);
        }
        driver
    }

    fn commit(&mut self, decision: Decision<Abstract>) -> (Outcome, Vec<Effect<Abstract>>) {
        for record in &decision.records {
            self.core
                .apply(record)
                .expect("the core applies every record it decided");
            self.log.push(record.clone());
        }
        (decision.outcome, decision.effects)
    }

    fn send(&mut self, event: Event<Abstract>) -> (Outcome, Vec<Effect<Abstract>>) {
        let decision = self.core.decide(event);
        self.commit(decision)
    }

    /// Crash and recover from the durable log, then resume.
    fn crash(&mut self) -> Vec<Effect<Abstract>> {
        self.core = Core::recover(self.limits.clone(), &self.log).expect("the log recovers");
        let decision = self.core.resume(Some(born()), None);
        self.commit(decision).1
    }

    fn since(&self, from: usize) -> Vec<Record<Abstract>> {
        self.log.get(from..).unwrap_or_default().to_vec()
    }

    /// Starts the first turn with message 1 and returns the first model
    /// call's id.
    fn start(&mut self) -> EffectId {
        let (outcome, effects) = self.send(deliver(1, 10));
        assert_eq!(
            outcome,
            Outcome::Accepted,
            "the first message starts a turn"
        );
        model_call(&effects)
    }

    /// Finishes model call `id` proposing `calls`.
    fn finish(
        &mut self,
        id: EffectId,
        calls: Vec<Proposal<Abstract>>,
    ) -> (Outcome, Vec<Effect<Abstract>>) {
        self.send(Event::ModelDone {
            id,
            result: ModelExit::Finished(calls),
        })
    }

    fn approve(&mut self, name: &str, verdict: Verdict) -> (Outcome, Vec<Effect<Abstract>>) {
        self.send(Event::Approve {
            call: call_id(name),
            verdict,
        })
    }

    fn tool_done(&mut self, id: EffectId, exit: Exit) -> (Outcome, Vec<Effect<Abstract>>) {
        self.send(Event::ToolDone {
            id,
            exit,
            output: payload(3),
        })
    }
}

fn model_call(effects: &[Effect<Abstract>]) -> EffectId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::CallModel { id, .. } => Some(*id),
            Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::RunTool { .. }
            | Effect::CancelTool { .. }
            | Effect::Sleep { .. }
            | Effect::Stop { .. } => None,
        })
        .expect("a CallModel effect")
}

/// The `RunTool` effects as (effect id, call id).
fn tool_runs(effects: &[Effect<Abstract>]) -> Vec<(EffectId, CallId)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::RunTool { call } => Some((call.effect(), call.id().clone())),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::CancelTool { .. }
            | Effect::Sleep { .. }
            | Effect::Stop { .. } => None,
        })
        .collect()
}

/// The single `RunTool` effect's id, checking it runs `name`.
fn single_run(effects: &[Effect<Abstract>], name: &str) -> EffectId {
    let runs = tool_runs(effects);
    assert_eq!(runs.len(), 1, "exactly one tool starts: {effects:?}");
    let (id, call) = runs.into_iter().next().expect("one run");
    assert_eq!(call, call_id(name), "the expected call runs");
    id
}

fn asked(effects: &[Effect<Abstract>]) -> Vec<CallId> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::AskApproval { request } => Some(request.id().clone()),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::RunTool { .. }
            | Effect::CancelTool { .. }
            | Effect::Sleep { .. }
            | Effect::Stop { .. } => None,
        })
        .collect()
}

const fn ended(outcome: TurnOutcome) -> Record<Abstract> {
    Record::TurnEnded {
        turn: FIRST,
        outcome,
    }
}

const fn result(call: usize, exit: Exit, output: Option<Measured<Unit>>) -> Record<Abstract> {
    Record::ToolResult {
        call: CallIx(call),
        exit,
        output,
    }
}

// ---------------------------------------------------------------------------
// Text turns and message deduplication
// ---------------------------------------------------------------------------

#[test]
fn text_turn_completes() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    assert_eq!(
        call,
        EffectId {
            epoch: Epoch(0),
            seq: 4
        },
        "the model call is named by its StepStarted record"
    );
    let (outcome, _) = driver.send(Event::ModelParts {
        id: call,
        parts: payload(5),
    });
    assert_eq!(outcome, Outcome::Accepted, "live parts are recorded");
    let (outcome, effects) = driver.finish(call, Vec::new());
    assert_eq!(outcome, Outcome::Accepted, "the model finished");
    assert!(effects.is_empty(), "nothing runs after the turn ends");
    assert_eq!(
        driver.since(2),
        vec![
            delivered(1, 10),
            Record::TurnStarted { turn: FIRST },
            first_step(),
            Record::ModelOutput {
                step: step(0),
                parts: payload(5)
            },
            Record::StepFinished {
                step: step(0),
                calls: Vec::new()
            },
            ended(TurnOutcome::Completed),
        ],
        "the log of a text turn"
    );
    assert_eq!(driver.core.running(), None, "no turn is running");
}

/// T3: a repeat is `Exists` and records nothing; a second message to a
/// busy actor waits in its mailbox, and the turn's end starts the next
/// turn, whose first model call carries it.
#[test]
fn repeated_message_exists_and_second_waits_its_turn() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let length = driver.log.len();
    let (outcome, _) = driver.send(deliver(1, 10));
    assert_eq!(outcome, Outcome::Exists, "a repeat is answered with exists");
    assert_eq!(driver.log.len(), length, "a repeat writes nothing");
    let (outcome, effects) = driver.send(deliver(2, 10));
    assert_eq!(outcome, Outcome::Accepted, "the mailbox takes it");
    assert!(effects.is_empty(), "the model call in flight goes on");
    assert_eq!(driver.since(length), vec![delivered(2, 10)], "it waits");
    assert_eq!(driver.core.unread(), 1, "unread");
    let from = driver.log.len();
    let (_, effects) = driver.finish(call, Vec::new());
    let next = turn(u64::try_from(from).expect("a seq") + 2);
    assert_eq!(
        driver.since(from),
        vec![
            Record::StepFinished {
                step: step(0),
                calls: Vec::new()
            },
            ended(TurnOutcome::Completed),
            Record::TurnStarted { turn: next },
            Record::StepStarted {
                step: StepId {
                    turn: next,
                    index: 0
                },
                carries: vec![EnvelopeId::User(message(2))],
            },
        ],
        "the turn's end starts the next, carrying the waiting message"
    );
    model_call(&effects);
    assert_eq!(driver.core.unread(), 0, "consumed");
}

#[test]
fn the_mailbox_is_bounded() {
    let mut driver = Driver::new(small_limits());
    for number in 1..=4 {
        let (outcome, effects) = driver.send(deliver(number, 1));
        assert_eq!(outcome, Outcome::Accepted, "message {number} fits");
        driver.finish(model_call(&effects), Vec::new());
    }
    let length = driver.log.len();
    let (outcome, _) = driver.send(deliver(5, 1));
    assert_eq!(
        outcome,
        Outcome::Rejected(Rejection::Limit),
        "messages_per_actor is a hard bound"
    );
    assert_eq!(driver.log.len(), length, "and records nothing");
}

// ---------------------------------------------------------------------------
// Tools and approvals
// ---------------------------------------------------------------------------

#[test]
fn safe_tool_is_auto_approved_and_logged() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let from = driver.log.len();
    let (_, effects) = driver.finish(call, vec![propose("read", Tool::Read)]);
    assert_eq!(
        driver.since(from),
        vec![
            Record::StepFinished {
                step: step(0),
                calls: vec![propose("read", Tool::Read)]
            },
            Record::Approved {
                call: CallIx(0),
                by: Grant::Rule { rule: 0, rules: 0 }
            },
            Record::Dispatched { call: CallIx(0) },
        ],
        "approval precedes dispatch, both before the run"
    );
    let run = single_run(&effects, "read");
    let from = driver.log.len();
    let (_, effects) = driver.tool_done(run, Exit::Ok);
    assert_eq!(
        driver.since(from),
        vec![result(0, Exit::Ok, Some(payload(3))), started(1)],
        "the result feeds the next step"
    );
    let (_, effects) = driver.finish(model_call(&effects), Vec::new());
    assert!(effects.is_empty(), "turn over");
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Completed)),
        "completed"
    );
}

#[test]
fn mutating_tool_waits_for_the_user() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(call, vec![propose("edit", Tool::Edit)]);
    assert_eq!(asked(&effects), vec![call_id("edit")], "the user is asked");
    assert!(tool_runs(&effects).is_empty(), "nothing runs unapproved");

    let (outcome, _) = driver.approve("other", Verdict::Approve);
    assert_eq!(
        outcome,
        Outcome::Rejected(Rejection::UnknownCall),
        "unknown id"
    );

    let from = driver.log.len();
    let (outcome, effects) = driver.approve("edit", Verdict::Approve);
    assert_eq!(outcome, Outcome::Accepted, "approved");
    assert_eq!(
        driver.since(from),
        vec![
            Record::Approved {
                call: CallIx(0),
                by: Grant::User
            },
            Record::Dispatched { call: CallIx(0) },
        ],
        "approval is durable before the run"
    );
    single_run(&effects, "edit");
    let (outcome, effects) = driver.approve("edit", Verdict::Approve);
    assert_eq!(
        outcome,
        Outcome::Rejected(Rejection::UnknownCall),
        "a repeated approval finds nothing awaiting it"
    );
    assert!(effects.is_empty(), "and starts nothing");
}

#[test]
fn denied_call_resolves_without_running() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.finish(call, vec![propose("bash", Tool::Bash)]);
    let from = driver.log.len();
    let (_, effects) = driver.approve("bash", Verdict::Deny);
    assert!(tool_runs(&effects).is_empty(), "denied calls never run");
    assert_eq!(
        driver.since(from),
        vec![
            result(0, Exit::NotStarted(Unstarted::Denied), None),
            started(1)
        ],
        "the model hears about the denial"
    );
}

#[test]
fn invalid_request_fails_without_running() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let invalid = Proposal {
        id: call_id("bogus"),
        request: Request::Invalid { reason: Unit },
    };
    let from = driver.log.len();
    let (_, effects) = driver.finish(call, vec![invalid.clone()]);
    assert_eq!(
        driver.since(from),
        vec![
            Record::StepFinished {
                step: step(0),
                calls: vec![invalid]
            },
            result(0, Exit::NotStarted(Unstarted::Invalid), None),
            started(1),
        ],
        "an invalid call fails and the next step starts"
    );
    model_call(&effects);
}

#[test]
fn approved_calls_run_at_once_beside_anything() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(
        call,
        vec![
            propose("edit-a", Tool::Edit),
            propose("edit-b", Tool::Edit),
            propose("bash", Tool::Bash),
            propose("read", Tool::Read),
        ],
    );
    let read = single_run(&effects, "read");
    let (_, effects) = driver.approve("edit-a", Verdict::Approve);
    let edit_a = single_run(&effects, "edit-a");
    let (_, effects) = driver.approve("bash", Verdict::Approve);
    let bash = single_run(&effects, "bash");
    let from = driver.log.len();
    let (_, effects) = driver.approve("edit-b", Verdict::Approve);
    let edit_b = single_run(&effects, "edit-b");
    assert_eq!(
        driver.since(from),
        vec![
            Record::Approved {
                call: CallIx(1),
                by: Grant::User
            },
            Record::Dispatched { call: CallIx(1) },
        ],
        "a second edit runs beside the first, bash and the read"
    );
    for id in [read, edit_a, bash] {
        let (_, effects) = driver.tool_done(id, Exit::Ok);
        assert!(effects.is_empty(), "the step waits for every result");
    }
    let (_, effects) = driver.tool_done(edit_b, Exit::Ok);
    model_call(&effects);
}

#[test]
fn safe_tools_run_together() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(
        call,
        vec![propose("read-a", Tool::Read), propose("read-b", Tool::Read)],
    );
    assert_eq!(tool_runs(&effects).len(), 2, "both reads start at once");
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

#[test]
fn interrupt_during_model_call() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (outcome, effects) = driver.send(Event::Interrupt { turn: FIRST });
    assert_eq!(outcome, Outcome::Accepted, "interrupt accepted");
    assert_eq!(
        effects,
        vec![Effect::CancelModel { id: call }],
        "the stream is cancelled"
    );
    let (outcome, _) = driver.send(Event::Interrupt { turn: FIRST });
    assert_eq!(outcome, Outcome::Exists, "a second interrupt is a repeat");
    let (outcome, _) = driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(ModelFault::Cancelled),
    });
    assert_eq!(outcome, Outcome::Accepted, "the model reports back");
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Cancelled)),
        "cancelled"
    );
    let (outcome, _) = driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(ModelFault::Cancelled),
    });
    assert_eq!(outcome, Outcome::Stale, "a late duplicate is stale");
    let ends = driver
        .log
        .iter()
        .filter(|record| matches!(record, Record::TurnEnded { .. }))
        .count();
    assert_eq!(ends, 1, "the turn ends exactly once");
}

#[test]
fn interrupt_wins_over_a_model_that_finished_anyway() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.send(Event::Interrupt { turn: FIRST });
    let from = driver.log.len();
    let (_, effects) = driver.finish(call, Vec::new());
    assert!(effects.is_empty(), "nothing more runs");
    assert_eq!(
        driver.since(from),
        vec![
            Record::StepFinished {
                step: step(0),
                calls: Vec::new()
            },
            ended(TurnOutcome::Cancelled),
        ],
        "what arrived is recorded, and Esc wins: the turn is cancelled"
    );
}

#[test]
fn interrupt_wins_over_proposed_calls_that_arrive_late() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.send(Event::Interrupt { turn: FIRST });
    let from = driver.log.len();
    let (_, effects) = driver.finish(
        call,
        vec![propose("read", Tool::Read), propose("edit", Tool::Edit)],
    );
    assert!(tool_runs(&effects).is_empty(), "nothing runs after Esc");
    assert!(asked(&effects).is_empty(), "nobody is asked");
    assert_eq!(
        driver.since(from),
        vec![
            Record::StepFinished {
                step: step(0),
                calls: vec![propose("read", Tool::Read), propose("edit", Tool::Edit)]
            },
            result(0, Exit::NotStarted(Unstarted::Cancelled), None),
            result(1, Exit::NotStarted(Unstarted::Cancelled), None),
            ended(TurnOutcome::Cancelled),
        ],
        "the calls are recorded and resolved as interrupted"
    );
}

#[test]
fn interrupt_during_tools() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(
        call,
        vec![propose("read", Tool::Read), propose("edit", Tool::Edit)],
    );
    let read = single_run(&effects, "read");
    let from = driver.log.len();
    let (_, effects) = driver.send(Event::Interrupt { turn: FIRST });
    assert_eq!(
        effects,
        vec![Effect::CancelTool { id: read }],
        "the running tool is asked to stop"
    );
    assert_eq!(
        driver.since(from),
        vec![
            Record::CancelRequested { turn: FIRST },
            result(1, Exit::NotStarted(Unstarted::Cancelled), None)
        ],
        "the unapproved call never runs"
    );
    let (outcome, _) = driver.approve("edit", Verdict::Approve);
    assert_eq!(
        outcome,
        Outcome::Rejected(Rejection::UnknownCall),
        "too late to approve"
    );
    let from = driver.log.len();
    driver.tool_done(read, Exit::Ok);
    assert_eq!(
        driver.since(from),
        vec![
            result(0, Exit::Ok, Some(payload(3))),
            ended(TurnOutcome::Cancelled)
        ],
        "the read finished anyway; the turn is cancelled once it did"
    );
}

/// C3, signal once: a second interrupt of a turn already stopping is a
/// repeat, and asks no run to stop again.
#[test]
fn a_second_interrupt_signals_nothing() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(call, vec![propose("read", Tool::Read)]);
    let read = single_run(&effects, "read");
    let (_, effects) = driver.send(Event::Interrupt { turn: FIRST });
    assert_eq!(
        effects,
        vec![Effect::CancelTool { id: read }],
        "signalled once"
    );
    let from = driver.log.len();
    let (outcome, effects) = driver.send(Event::Interrupt { turn: FIRST });
    assert_eq!(outcome, Outcome::Exists, "a repeat");
    assert!(effects.is_empty(), "no second signal");
    assert_eq!(driver.since(from), Vec::new(), "nothing recorded");
}

/// T2, the deadline first: v stops waiting, records `Unknown` and asks the
/// run to stop; the run's late report changes nothing.
#[test]
fn a_deadline_before_the_report_resolves_unknown() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.finish(call, vec![propose("bash", Tool::Bash)]);
    let (_, effects) = driver.approve("bash", Verdict::Approve);
    let bash = single_run(&effects, "bash");
    let from = driver.log.len();
    let (outcome, effects) = driver.send(Event::Expired { id: bash });
    assert_eq!(outcome, Outcome::Accepted, "the deadline is live");
    assert!(
        effects.contains(&Effect::CancelTool { id: bash }),
        "the run is asked to stop: {effects:?}"
    );
    assert_eq!(
        driver.since(from),
        vec![result(0, Exit::Unknown, None), started(1),],
        "it may have acted: Unknown"
    );
    let (outcome, _) = driver.tool_done(bash, Exit::Ok);
    assert_eq!(outcome, Outcome::Stale, "the known outcome stays");
    let (outcome, _) = driver.send(Event::Expired { id: bash });
    assert_eq!(outcome, Outcome::Stale, "and so does a repeated deadline");
}

/// T2, the report first: its outcome stands, and the deadline is stale.
#[test]
fn a_report_before_the_deadline_wins() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.finish(call, vec![propose("bash", Tool::Bash)]);
    let (_, effects) = driver.approve("bash", Verdict::Approve);
    let bash = single_run(&effects, "bash");
    driver.tool_done(bash, Exit::Failed);
    let from = driver.log.len();
    let (outcome, effects) = driver.send(Event::Expired { id: bash });
    assert_eq!(outcome, Outcome::Stale, "nothing is running");
    assert!(effects.is_empty(), "no signal");
    assert_eq!(driver.since(from), Vec::new(), "the Failed result stands");
}

/// C3: a deadline during a cancel resolves the run and ends the turn; no
/// step starts.
#[test]
fn a_deadline_under_a_cancel_starts_nothing() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(call, vec![propose("read", Tool::Read)]);
    let read = single_run(&effects, "read");
    driver.send(Event::Interrupt { turn: FIRST });
    let from = driver.log.len();
    let (_, effects) = driver.send(Event::Expired { id: read });
    assert!(tool_runs(&effects).is_empty(), "nothing runs");
    assert_eq!(
        driver.since(from),
        vec![
            result(0, Exit::Unknown, None),
            ended(TurnOutcome::Cancelled)
        ],
        "the turn ends cancelled"
    );
}

#[test]
fn interrupt_during_backoff_ends_at_once() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(ModelFault::Overloaded),
    });
    let sleep = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Sleep { id, .. } => Some(*id),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::RunTool { .. }
            | Effect::CancelTool { .. }
            | Effect::Stop { .. } => None,
        })
        .expect("a backoff");
    driver.send(Event::Interrupt { turn: FIRST });
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Cancelled)),
        "cancelled"
    );
    let (outcome, _) = driver.send(Event::Woke { id: sleep });
    assert_eq!(outcome, Outcome::Stale, "the timer no longer matters");
}

// ---------------------------------------------------------------------------
// Retries
// ---------------------------------------------------------------------------

#[test]
fn transient_model_failures_retry_on_schedule() {
    let mut driver = Driver::new(small_limits());
    let mut call = driver.start();
    for (fault, delay) in [(ModelFault::Overloaded, 10), (ModelFault::Network, 20)] {
        let from = driver.log.len();
        let (_, effects) = driver.send(Event::ModelDone {
            id: call,
            result: ModelExit::Failed(fault),
        });
        let reset = Record::StepReset {
            step: step(0),
            reason: Reset::Retry {
                fault,
                delay: Millis::new(delay),
            },
        };
        assert_eq!(driver.since(from), vec![reset], "a reset-step marker");
        let (id, wait) = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Sleep { id, delay } => Some((*id, *delay)),
                Effect::CallModel { .. }
                | Effect::CancelModel { .. }
                | Effect::AskApproval { .. }
                | Effect::RunTool { .. }
                | Effect::CancelTool { .. }
                | Effect::Stop { .. } => None,
            })
            .expect("a backoff");
        assert_eq!(wait, Millis::new(delay), "the schedule is data");
        let (_, effects) = driver.send(Event::Woke { id });
        assert_eq!(
            driver.log.last(),
            Some(&started(0)),
            "the same step again, carrying nothing more"
        );
        let next = model_call(&effects);
        assert_ne!(next, call, "a fresh effect id per attempt");
        let (outcome, _) = driver.send(Event::ModelParts {
            id: call,
            parts: payload(1),
        });
        assert_eq!(outcome, Outcome::Stale, "the old attempt is dead");
        call = next;
    }
    driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(ModelFault::Overloaded),
    });
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Model(
            ModelFault::Overloaded
        )))),
        "the budget is spent: the turn fails"
    );
}

#[test]
fn other_model_failures_do_not_retry() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(ModelFault::Other),
    });
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Model(
            ModelFault::Other
        )))),
        "failed"
    );
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn steps_per_turn_is_a_bound() {
    let mut driver = Driver::new(Limits::new(LimitsConfig {
        steps_per_turn: NonZeroU32::new(1).expect("non-zero"),
        ..small_config()
    }));
    let call = driver.start();
    let (_, effects) = driver.finish(call, vec![propose("read", Tool::Read)]);
    let read = single_run(&effects, "read");
    driver.tool_done(read, Exit::Ok);
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Limit(Limit::Steps)))),
        "no second step"
    );
}

#[test]
fn context_is_checked_before_sending() {
    let mut driver = Driver::new(small_limits());
    let (_, effects) = driver.send(deliver(1, 5_000));
    assert!(effects.is_empty(), "the model is never called");
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Limit(Limit::Context)))),
        "context limit"
    );
}

#[test]
fn a_discarded_attempt_does_not_count_against_context() {
    let mut driver = Driver::new(small_limits());
    let first = driver.start();
    driver.send(Event::ModelParts {
        id: first,
        parts: payload(600),
    });
    let (_, effects) = driver.send(Event::ModelDone {
        id: first,
        result: ModelExit::Failed(ModelFault::Network),
    });
    let sleep = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Sleep { id, .. } => Some(*id),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::RunTool { .. }
            | Effect::CancelTool { .. }
            | Effect::Stop { .. } => None,
        })
        .expect("a backoff");
    let (_, effects) = driver.send(Event::Woke { id: sleep });
    let second = model_call(&effects);
    driver.send(Event::ModelParts {
        id: second,
        parts: payload(600),
    });
    let (_, effects) = driver.finish(second, vec![propose("read", Tool::Read)]);
    let read = single_run(&effects, "read");
    let (_, effects) = driver.tool_done(read, Exit::Ok);
    assert_ne!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Limit(Limit::Context)))),
        "only the attempt that was kept counts"
    );
    model_call(&effects);
}

#[test]
fn the_turn_budget_counts_proposals() {
    let mut driver = Driver::new(Limits::new(LimitsConfig {
        calls_per_step: count(1),
        calls_per_turn: count(2),
        ..small_config()
    }));
    let call = driver.start();
    let (_, effects) = driver.finish(
        call,
        vec![propose("read-a", Tool::Read), propose("read-b", Tool::Read)],
    );
    let read = single_run(&effects, "read-a");
    let (_, effects) = driver.tool_done(read, Exit::Ok);
    let from = driver.log.len();
    let (_, effects) = driver.finish(model_call(&effects), vec![propose("read-c", Tool::Read)]);
    assert!(
        tool_runs(&effects).is_empty(),
        "one call ran, but two were proposed: the turn's budget is spent"
    );
    assert!(
        driver
            .since(from)
            .contains(&result(2, Exit::NotStarted(Unstarted::Limit), None)),
        "step 2's read fails by the limit"
    );
}

#[test]
fn calls_past_the_step_bound_are_recorded_and_fail() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let calls: Vec<Proposal<Abstract>> = (0u8..5)
        .map(|number| propose(&format!("read-{number}"), Tool::Read))
        .collect();
    let from = driver.log.len();
    let (_, effects) = driver.finish(call, calls.clone());
    assert_eq!(
        driver.log.get(from),
        Some(&Record::StepFinished {
            step: step(0),
            calls
        }),
        "every call the model made is recorded"
    );
    assert!(
        driver
            .since(from)
            .contains(&result(4, Exit::NotStarted(Unstarted::Limit), None)),
        "the call past calls_per_step fails by the limit"
    );
    let runs = tool_runs(&effects);
    assert_eq!(runs.len(), 4, "the four within the bound run");
    let mut last = Vec::new();
    for (id, _) in runs {
        last = driver.tool_done(id, Exit::Ok).1;
    }
    model_call(&last);
}

#[test]
fn calls_past_the_turn_bound_fail() {
    let mut driver = Driver::new(Limits::new(LimitsConfig {
        calls_per_turn: count(1),
        ..small_config()
    }));
    let call = driver.start();
    let (_, effects) = driver.finish(
        call,
        vec![propose("read-a", Tool::Read), propose("read-b", Tool::Read)],
    );
    let read = single_run(&effects, "read-a");
    assert!(
        driver
            .log
            .contains(&result(1, Exit::NotStarted(Unstarted::Limit), None)),
        "the second call is past calls_per_turn"
    );
    let (_, effects) = driver.tool_done(read, Exit::Ok);
    let (_, effects) = driver.finish(model_call(&effects), vec![propose("read-c", Tool::Read)]);
    assert!(tool_runs(&effects).is_empty(), "the turn's budget is spent");
    model_call(&effects);
}

#[test]
fn reused_call_ids_fail_the_step() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let from = driver.log.len();
    let (_, effects) = driver.finish(
        call,
        vec![propose("same", Tool::Read), propose("same", Tool::Edit)],
    );
    assert!(effects.is_empty(), "nothing runs");
    assert_eq!(
        driver.since(from),
        vec![ended(TurnOutcome::Failed(Failure::Model(
            ModelFault::Other
        )))],
        "never two live calls with one id"
    );
}

// ---------------------------------------------------------------------------
// Crash and resume
// ---------------------------------------------------------------------------

#[test]
fn crash_makes_dispatched_bash_unknown() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.finish(call, vec![propose("bash", Tool::Bash)]);
    let (_, effects) = driver.approve("bash", Verdict::Approve);
    let bash = single_run(&effects, "bash");
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(tool_runs(&effects).is_empty(), "bash is never rerun");
    assert_eq!(
        driver.since(from),
        vec![
            Record::EpochBumped { epoch: Epoch(1) },
            result(0, Exit::Unknown, None),
            started(1),
        ],
        "its outcome is unknown and the model is told"
    );
    let (outcome, _) = driver.tool_done(bash, Exit::Ok);
    assert_eq!(outcome, Outcome::Stale, "a reply from before the crash");
}

#[test]
fn crash_makes_dispatched_edit_unknown() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.finish(call, vec![propose("edit", Tool::Edit)]);
    let (_, effects) = driver.approve("edit", Verdict::Approve);
    let edit = single_run(&effects, "edit");
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(tool_runs(&effects).is_empty(), "an edit is never rerun");
    assert_eq!(
        driver.since(from),
        vec![
            Record::EpochBumped { epoch: Epoch(1) },
            result(0, Exit::Unknown, None),
            started(1),
        ],
        "its outcome is unknown and the model is told"
    );
    let (outcome, _) = driver.tool_done(edit, Exit::Ok);
    assert_eq!(outcome, Outcome::Stale, "a reply from before the crash");
}

#[test]
fn crash_reruns_a_safe_tool_and_resolves_the_rest_unknown() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(
        call,
        vec![propose("read", Tool::Read), propose("bash", Tool::Bash)],
    );
    let before = single_run(&effects, "read");
    let (_, effects) = driver.approve("bash", Verdict::Approve);
    single_run(&effects, "bash");
    let from = driver.log.len();
    let effects = driver.crash();
    let after = single_run(&effects, "read");
    assert_eq!(
        after,
        EffectId {
            epoch: Epoch(1),
            seq: before.seq
        },
        "same licensing record, new epoch"
    );
    assert_eq!(
        driver.since(from),
        vec![
            Record::EpochBumped { epoch: Epoch(1) },
            result(1, Exit::Unknown, None),
        ],
        "bash is unknown; the read runs again"
    );
    let dispatches = driver
        .log
        .iter()
        .filter(|record| matches!(record, Record::Dispatched { .. }))
        .count();
    assert_eq!(dispatches, 2, "a rerun is not a second dispatch");
    let (outcome, _) = driver.tool_done(before, Exit::Ok);
    assert_eq!(outcome, Outcome::Stale, "old epoch");
    let (outcome, effects) = driver.tool_done(after, Exit::Ok);
    assert_eq!(outcome, Outcome::Accepted, "current epoch");
    model_call(&effects);
}

#[test]
fn crash_restarts_model_step() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.send(Event::ModelParts {
        id: call,
        parts: payload(4),
    });
    let from = driver.log.len();
    let effects = driver.crash();
    assert_eq!(
        driver.since(from),
        vec![
            Record::EpochBumped { epoch: Epoch(1) },
            Record::StepReset {
                step: step(0),
                reason: Reset::Restart
            },
            started(0),
        ],
        "partial output is reset and the step runs again"
    );
    assert_eq!(model_call(&effects).epoch, Epoch(1), "new epoch");
}

/// T2: a crash while the turn is being cancelled leaves a dispatched read
/// `Unknown`, never `NotStarted`: it was dispatched, so it may have acted.
#[test]
fn a_crash_while_cancelling_leaves_a_dispatched_read_unknown() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.finish(call, vec![propose("read", Tool::Read)]);
    single_run(&effects, "read");
    driver.send(Event::Interrupt { turn: FIRST });
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(
        tool_runs(&effects).is_empty(),
        "nothing reruns under a cancel"
    );
    assert_eq!(
        driver.since(from),
        vec![
            Record::EpochBumped { epoch: Epoch(1) },
            result(0, Exit::Unknown, None),
            ended(TurnOutcome::Cancelled),
        ],
        "the read may have run: its outcome is unknown"
    );
}

/// T2: the log refuses a result that claims more than v knows.
#[test]
fn a_log_claiming_more_than_v_knows_is_corrupt() {
    let mut dispatched = proposed(vec![propose("bash", Tool::Bash)]);
    dispatched.push(Record::Approved {
        call: CallIx(0),
        by: Grant::User,
    });
    dispatched.push(Record::Dispatched { call: CallIx(0) });
    let mut never_started = dispatched.clone();
    never_started.push(result(0, Exit::NotStarted(Unstarted::Cancelled), None));
    assert_eq!(
        Core::recover(small_limits(), &never_started),
        Err(Corrupt {
            seq: 8,
            violation: Violation::WrongResult
        }),
        "v cannot know a dispatched call never acted"
    );
    let mut reported = dispatched.clone();
    reported.push(result(
        0,
        Exit::NotStarted(Unstarted::Cancelled),
        Some(payload(1)),
    ));
    assert!(
        Core::recover(small_limits(), &reported).is_ok(),
        "its run may report that it never began"
    );
    let mut unknown_before_dispatch = dispatched;
    unknown_before_dispatch.truncate(7);
    unknown_before_dispatch.push(result(0, Exit::Unknown, None));
    assert_eq!(
        Core::recover(small_limits(), &unknown_before_dispatch),
        Err(Corrupt {
            seq: 7,
            violation: Violation::WrongResult
        }),
        "an undispatched call provably never acted"
    );
}

#[test]
fn crash_asks_again_for_pending_approvals() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.finish(call, vec![propose("edit", Tool::Edit)]);
    let effects = driver.crash();
    assert_eq!(
        asked(&effects),
        vec![call_id("edit")],
        "the approval prompt survives a crash"
    );
}

#[test]
fn every_prefix_of_a_decision_recovers() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let durable = driver.log.len();
    driver.finish(
        call,
        vec![propose("read", Tool::Read), propose("edit", Tool::Edit)],
    );
    for keep in durable..=driver.log.len() {
        let mut torn = Driver::new(small_limits());
        torn.log = driver.log.get(..keep).expect("a prefix").to_vec();
        let effects = torn.crash();
        if keep > durable {
            assert_eq!(
                tool_runs(&effects).len(),
                1,
                "the read runs whatever survived (kept {keep})"
            );
            assert_eq!(
                asked(&effects),
                vec![call_id("edit")],
                "the edit is asked (kept {keep})"
            );
        } else {
            model_call(&effects);
        }
    }
}

#[test]
fn corrupt_log_stops_recovery() {
    let mut log = proposed(vec![propose("bash", Tool::Bash)]);
    log.push(Record::Dispatched { call: CallIx(0) });
    assert_eq!(
        Core::recover(small_limits(), &log),
        Err(Corrupt {
            seq: 6,
            violation: Violation::WrongPhase
        }),
        "dispatch without approval is refused"
    );
    let mut cancelled_then_completed = boot();
    cancelled_then_completed.extend([
        delivered(1, 1),
        Record::TurnStarted { turn: FIRST },
        Record::CancelRequested { turn: FIRST },
        ended(TurnOutcome::Completed),
    ]);
    assert_eq!(
        Core::recover(small_limits(), &cancelled_then_completed),
        Err(Corrupt {
            seq: 5,
            violation: Violation::WrongEnding
        }),
        "a turn the user stopped cannot end completed"
    );
}

/// The start of a log whose first step proposed `calls`: the boot (rules
/// at 0), message 1, its turn and step.
fn proposed(calls: Vec<Proposal<Abstract>>) -> Vec<Record<Abstract>> {
    let mut log = boot();
    log.extend([
        delivered(1, 1),
        Record::TurnStarted { turn: FIRST },
        first_step(),
        Record::StepFinished {
            step: step(0),
            calls,
        },
    ]);
    log
}

/// T3 (a command accepted once): a log delivering one envelope twice is
/// refused.
#[test]
fn a_log_repeating_a_message_is_corrupt() {
    let mut log = proposed(Vec::new());
    log.push(ended(TurnOutcome::Completed));
    log.push(delivered(1, 1));
    assert_eq!(
        Core::recover(small_limits(), &log),
        Err(Corrupt {
            seq: 7,
            violation: Violation::DuplicateEnvelope
        }),
        "one message is delivered once"
    );
}

/// C1, on the log: a grant names a rule in force that fits the call.
#[test]
fn a_grant_names_a_rule_in_force_that_fits() {
    let mut log = proposed(vec![propose("edit", Tool::Edit)]);
    log.push(Record::Approved {
        call: CallIx(0),
        by: Grant::Rule { rule: 0, rules: 0 },
    });
    assert_eq!(
        Core::recover(small_limits(), &log),
        Err(Corrupt {
            seq: 6,
            violation: Violation::WrongGrant
        }),
        "`read: allow` does not grant an edit"
    );
    let read = proposed(vec![propose("read", Tool::Read)]);
    let mut granted = read.clone();
    granted.push(Record::Approved {
        call: CallIx(0),
        by: Grant::Rule { rule: 0, rules: 0 },
    });
    assert!(
        Core::recover(small_limits(), &granted).is_ok(),
        "`read: allow` grants a read"
    );
    for by in [
        Grant::Rule { rule: 1, rules: 0 },
        Grant::Rule { rule: 0, rules: 1 },
    ] {
        let mut wrong = read.clone();
        wrong.push(Record::Approved {
            call: CallIx(0),
            by,
        });
        assert_eq!(
            Core::recover(small_limits(), &wrong),
            Err(Corrupt {
                seq: 6,
                violation: Violation::WrongGrant
            }),
            "{by:?} names no rule in force"
        );
    }
    let second = StepId {
        turn: turn(2),
        index: 0,
    };
    let unruled = vec![
        Record::Born { born: born() },
        delivered(1, 1),
        Record::TurnStarted { turn: turn(2) },
        Record::StepStarted {
            step: second,
            carries: vec![EnvelopeId::User(message(1))],
        },
        Record::StepFinished {
            step: second,
            calls: vec![propose("read", Tool::Read)],
        },
        Record::Approved {
            call: CallIx(0),
            by: Grant::Rule { rule: 0, rules: 0 },
        },
    ];
    assert_eq!(
        Core::recover(small_limits(), &unruled),
        Err(Corrupt {
            seq: 5,
            violation: Violation::WrongGrant
        }),
        "with no rules in force, only the user grants"
    );
}

#[test]
fn a_log_dispatching_calls_together_recovers() {
    for (first, second) in [
        (Tool::Edit, Tool::Edit),
        (Tool::Bash, Tool::Read),
        (Tool::Bash, Tool::Bash),
        (Tool::Edit, Tool::Bash),
    ] {
        let mut log = proposed(vec![propose("first", first), propose("second", second)]);
        for index in 0..2 {
            log.push(Record::Approved {
                call: CallIx(index),
                by: Grant::User,
            });
        }
        log.push(Record::Dispatched { call: CallIx(0) });
        log.push(Record::Dispatched { call: CallIx(1) });
        assert!(
            Core::recover(small_limits(), &log).is_ok(),
            "{first:?} beside {second:?}: the core has no concurrency rules"
        );
    }
}

// ---------------------------------------------------------------------------
// Record JSON shape
// ---------------------------------------------------------------------------

/// Real text in every slot, to pin the log's JSON shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Text;

impl Payload for Text {
    type Text = String;
    type Parts = String;
    type Args = Shape;
    type Reason = String;
    type Output = String;

    fn shape(args: &Shape) -> Shape {
        Abstract::shape(args)
    }

    fn tool(args: &Shape) -> Tool {
        args.tool()
    }
}

#[test]
fn record_json_shape() {
    let turn = TurnId(3);
    let message = MessageId::parse("0190a8e2-7c3b-7d4e-8f00-0123456789ab").expect("a v7");
    let actor = ActorId::parse("0190a8e2-7c3b-7d4e-8f00-0123456789ac").expect("a v7");
    let records: Vec<Record<Text>> = vec![
        Record::Delivered {
            envelope: Envelope {
                id: EnvelopeId::User(message),
                from: Address::User,
                to: actor,
                reply_to: None,
                body: Body::Text(Measured {
                    body: "hi".to_owned(),
                    bytes: 2,
                }),
            },
        },
        Record::TurnStarted { turn },
        Record::StepStarted {
            step: StepId { turn, index: 0 },
            carries: vec![EnvelopeId::User(message)],
        },
        Record::StepFinished {
            step: StepId { turn, index: 0 },
            calls: vec![Proposal {
                id: call_id("toolu_01"),
                request: Request::Run {
                    args: Shape::Edit {
                        subject: b"a.txt".to_vec(),
                    },
                },
            }],
        },
        Record::Approved {
            call: CallIx(0),
            by: Grant::User,
        },
        Record::ToolResult {
            call: CallIx(0),
            exit: Exit::NotStarted(Unstarted::Limit),
            output: None,
        },
        Record::StepReset {
            step: StepId { turn, index: 0 },
            reason: Reset::Retry {
                fault: ModelFault::Overloaded,
                delay: Millis::new(500),
            },
        },
        Record::TurnEnded {
            turn,
            outcome: TurnOutcome::Failed(Failure::Limit(Limit::Context)),
        },
        Record::EpochBumped { epoch: Epoch(2) },
    ];
    let lines: Vec<String> = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("records serialize"))
        .collect();
    assert_eq!(
        lines,
        vec![
            r#"{"type":"Delivered","envelope":{"id":{"user":"0190a8e2-7c3b-7d4e-8f00-0123456789ab"},"from":"user","to":"0190a8e2-7c3b-7d4e-8f00-0123456789ac","replyTo":null,"body":{"text":{"body":"hi","bytes":2}}}}"#,
            r#"{"type":"TurnStarted","turn":"3"}"#,
            r#"{"type":"StepStarted","step":{"turn":"3","index":0},"carries":[{"user":"0190a8e2-7c3b-7d4e-8f00-0123456789ab"}]}"#,
            r#"{"type":"StepFinished","step":{"turn":"3","index":0},"calls":[{"id":"toolu_01","request":{"Run":{"args":{"tool":"edit","subject":[97,46,116,120,116]}}}}]}"#,
            r#"{"type":"Approved","call":0,"by":"User"}"#,
            r#"{"type":"ToolResult","call":0,"exit":{"NotStarted":"Limit"},"output":null}"#,
            r#"{"type":"StepReset","step":{"turn":"3","index":0},"reason":{"Retry":{"fault":"Overloaded","delay":500}}}"#,
            r#"{"type":"TurnEnded","turn":"3","outcome":{"Failed":{"Limit":"Context"}}}"#,
            r#"{"type":"EpochBumped","epoch":2}"#,
        ],
        "one JSON object per line, tagged by type"
    );
    for (line, record) in lines.iter().zip(&records) {
        let parsed: Record<Text> = serde_json::from_str(line).expect("records parse back");
        assert_eq!(&parsed, record, "round trip of {line}");
    }
    assert!(
        serde_json::from_str::<Record<Text>>(
            r#"{"type":"TurnEnded","turn":"03","outcome":"Completed"}"#
        )
        .is_err(),
        "a turn id that is not a seq in canonical decimal does not parse"
    );
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// An abstract input, resolved against whatever is outstanding.
#[derive(Clone, Debug)]
enum Choice {
    Message(u8),
    Approve(u8, bool),
    Interrupt,
    Parts(u8),
    ModelFinish(u8, u8),
    ModelFail(u8, u8),
    ToolDone(u8, bool),
    Woke(u8),
    Expire(u8),
    Crash,
}

fn choice() -> impl Strategy<Value = Choice> {
    prop_oneof![
        (0u8..3).prop_map(Choice::Message),
        (any::<u8>(), any::<bool>()).prop_map(|(pick, yes)| Choice::Approve(pick, yes)),
        Just(Choice::Interrupt),
        any::<u8>().prop_map(Choice::Parts),
        (any::<u8>(), 0u8..6).prop_map(|(pick, script)| Choice::ModelFinish(pick, script)),
        (any::<u8>(), 0u8..4).prop_map(|(pick, cause)| Choice::ModelFail(pick, cause)),
        (any::<u8>(), any::<bool>()).prop_map(|(pick, ok)| Choice::ToolDone(pick, ok)),
        any::<u8>().prop_map(Choice::Woke),
        any::<u8>().prop_map(Choice::Expire),
        Just(Choice::Crash),
    ]
}

fn pick<T: Clone>(pool: &[T], index: u8) -> Option<T> {
    match pool.len() {
        0 => None,
        length => usize::from(index)
            .checked_rem(length)
            .and_then(|at| pool.get(at))
            .cloned(),
    }
}

fn script(number: u8, salt: usize) -> Vec<Proposal<Abstract>> {
    let tools: &[Tool] = match number {
        0 => &[],
        1 => &[Tool::Read],
        2 => &[Tool::Edit],
        3 => &[Tool::Bash],
        4 => &[Tool::Edit, Tool::Bash, Tool::Read],
        _ => &[Tool::Read, Tool::Read, Tool::Read, Tool::Read, Tool::Read],
    };
    tools
        .iter()
        .enumerate()
        .map(|(position, tool)| propose(&format!("{salt}-{position}"), *tool))
        .collect()
}

/// Every id ever handed out, live or stale: stale ones must be refused.
#[derive(Default)]
struct Seen {
    models: Vec<EffectId>,
    tools: Vec<EffectId>,
    sleeps: Vec<EffectId>,
    asks: Vec<CallId>,
}

impl Seen {
    fn note(&mut self, effects: &[Effect<Abstract>]) {
        for effect in effects {
            match effect {
                Effect::CallModel { id, .. } => self.models.push(*id),
                Effect::RunTool { call } => self.tools.push(call.effect()),
                Effect::Sleep { id, .. } => self.sleeps.push(*id),
                Effect::AskApproval { request } => self.asks.push(request.id().clone()),
                Effect::CancelModel { .. } | Effect::CancelTool { .. } | Effect::Stop { .. } => {}
            }
        }
    }
}

const fn cause(number: u8) -> ModelFault {
    match number {
        0 => ModelFault::Overloaded,
        1 => ModelFault::Network,
        2 => ModelFault::Cancelled,
        _ => ModelFault::Other,
    }
}

fn event_for(choice: &Choice, seen: &Seen, driver: &Driver) -> Option<Event<Abstract>> {
    Some(match choice {
        Choice::Message(number) => deliver(u64::from(*number), u64::from(*number) * 100),
        Choice::Approve(index, yes) => Event::Approve {
            call: pick(&seen.asks, *index)?,
            verdict: if *yes {
                Verdict::Approve
            } else {
                Verdict::Deny
            },
        },
        Choice::Interrupt => Event::Interrupt {
            turn: driver.core.running()?,
        },
        Choice::Parts(index) => Event::ModelParts {
            id: pick(&seen.models, *index)?,
            parts: payload(7),
        },
        Choice::ModelFinish(index, number) => Event::ModelDone {
            id: pick(&seen.models, *index)?,
            result: ModelExit::Finished(script(*number, driver.log.len())),
        },
        Choice::ModelFail(index, number) => Event::ModelDone {
            id: pick(&seen.models, *index)?,
            result: ModelExit::Failed(cause(*number)),
        },
        Choice::ToolDone(index, ok) => Event::ToolDone {
            id: pick(&seen.tools, *index)?,
            exit: if *ok { Exit::Ok } else { Exit::Failed },
            output: payload(11),
        },
        Choice::Woke(index) => Event::Woke {
            id: pick(&seen.sleeps, *index)?,
        },
        Choice::Expire(index) => Event::Expired {
            id: pick(&seen.tools, *index)?,
        },
        Choice::Crash => return None,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Within bounds, decide never panics, apply accepts every record decide
    /// produced, and recovering from the log reproduces the live state.
    #[test]
    fn recovery_reproduces_live_state(choices in proptest::collection::vec(choice(), 0..80)) {
        let mut driver = Driver::new(small_limits());
        let mut seen = Seen::default();
        for choice in &choices {
            let effects = if matches!(choice, Choice::Crash) {
                driver.crash()
            } else if let Some(event) = event_for(choice, &seen, &driver) {
                driver.send(event).1
            } else {
                continue;
            };
            seen.note(&effects);
            let recovered = Core::recover(small_limits(), &driver.log);
            prop_assert_eq!(recovered.as_ref(), Ok(&driver.core), "recover(log) == live state");
        }
    }
}
