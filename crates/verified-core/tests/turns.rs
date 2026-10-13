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
    Abstract, Approver, CallId, CallIx, Cause, EffectId, Epoch, Exit, Failure, Fault, Limit,
    Limits, LimitsConfig, Measured, MessageId, Millis, Payload, Proposal, Record, Request, Reset,
    StepId, Tool, TurnId, TurnOutcome, Unit, UnixMillis,
};

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Bounds small enough to reach every limit in a test.
fn small_config() -> LimitsConfig {
    LimitsConfig {
        turns_per_session: count(4),
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

fn limits(config: LimitsConfig) -> Limits {
    Limits::parse(config).expect("consistent limits")
}

fn small_limits() -> Limits {
    limits(small_config())
}

const fn payload(bytes: u64) -> Measured<Unit> {
    Measured { body: Unit, bytes }
}

/// A distinct instant per number, so ids minted from it differ.
fn at(number: u64) -> UnixMillis {
    UnixMillis::new(1_700_000_000_000u64.saturating_add(number)).expect("a 48-bit time")
}

fn turn(number: u64) -> TurnId {
    TurnId::from_parts(at(number), &[1; 10])
}

fn message(number: u64) -> MessageId {
    MessageId::from_parts(at(number), &[2; 10])
}

fn step(index: u32) -> StepId {
    StepId {
        turn: turn(1),
        index,
    }
}

fn call_id(name: &str) -> CallId {
    CallId::parse(name).expect("a protocol id")
}

fn propose(name: &str, tool: Tool) -> Proposal<Abstract> {
    Proposal {
        id: call_id(name),
        request: Request::Run { args: tool },
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
    fn new(limits: Limits) -> Self {
        Self {
            core: Core::new(limits.clone()),
            limits,
            log: Vec::new(),
        }
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
        let decision = self.core.resume();
        self.commit(decision).1
    }

    fn since(&self, from: usize) -> Vec<Record<Abstract>> {
        self.log.get(from..).unwrap_or_default().to_vec()
    }

    /// Starts turn 1 and returns the first model call's id.
    fn start(&mut self) -> EffectId {
        let (outcome, effects) = self.send(Event::Message {
            message: message(1),
            turn: turn(1),
            text: payload(10),
        });
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
            | Effect::Sleep { .. } => None,
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
            | Effect::Sleep { .. } => None,
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
            | Effect::Sleep { .. } => None,
        })
        .collect()
}

fn ended(outcome: TurnOutcome) -> Record<Abstract> {
    Record::TurnEnded {
        turn: turn(1),
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
            seq: 1
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
        driver.log,
        vec![
            Record::TurnStarted {
                turn: turn(1),
                message: message(1),
                text: payload(10)
            },
            Record::StepStarted { step: step(0) },
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

#[test]
fn repeated_message_exists_and_second_is_busy() {
    let mut driver = Driver::new(small_limits());
    driver.start();
    let length = driver.log.len();
    let (outcome, _) = driver.send(Event::Message {
        message: message(1),
        turn: turn(1),
        text: payload(10),
    });
    assert_eq!(outcome, Outcome::Exists, "a repeat is answered with exists");
    let (outcome, _) = driver.send(Event::Message {
        message: message(2),
        turn: turn(2),
        text: payload(10),
    });
    assert_eq!(
        outcome,
        Outcome::Rejected(Rejection::Busy),
        "one turn at a time"
    );
    assert_eq!(driver.log.len(), length, "neither wrote anything");
}

#[test]
fn session_is_bounded() {
    let mut driver = Driver::new(small_limits());
    for number in 1..=4 {
        let (outcome, effects) = driver.send(Event::Message {
            message: message(number),
            turn: turn(number),
            text: payload(1),
        });
        assert_eq!(outcome, Outcome::Accepted, "message {number} fits");
        driver.finish(model_call(&effects), Vec::new());
    }
    let (outcome, _) = driver.send(Event::Message {
        message: message(5),
        turn: turn(5),
        text: payload(1),
    });
    assert_eq!(
        outcome,
        Outcome::Rejected(Rejection::SessionFull),
        "turns_per_session is a hard bound"
    );
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
                by: Approver::Policy
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
        vec![
            result(0, Exit::Ok, Some(payload(3))),
            Record::StepStarted { step: step(1) }
        ],
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
                by: Approver::User
            },
            Record::Dispatched { call: CallIx(0) },
        ],
        "approval is durable before the run"
    );
    single_run(&effects, "edit");
    let (outcome, effects) = driver.approve("edit", Verdict::Approve);
    assert_eq!(outcome, Outcome::Exists, "a repeated approval is a no-op");
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
            result(0, Exit::Err(Cause::Denied), None),
            Record::StepStarted { step: step(1) }
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
            result(0, Exit::Err(Cause::Failed(Fault::Other)), None),
            Record::StepStarted { step: step(1) },
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
                by: Approver::User
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
    let (outcome, effects) = driver.send(Event::Interrupt { turn: turn(1) });
    assert_eq!(outcome, Outcome::Accepted, "interrupt accepted");
    assert_eq!(
        effects,
        vec![Effect::CancelModel { id: call }],
        "the stream is cancelled"
    );
    let (outcome, _) = driver.send(Event::Interrupt { turn: turn(1) });
    assert_eq!(outcome, Outcome::Exists, "a second interrupt is a repeat");
    let (outcome, _) = driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(Cause::Interrupted),
    });
    assert_eq!(outcome, Outcome::Accepted, "the model reports back");
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Cancelled)),
        "cancelled"
    );
    let (outcome, _) = driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(Cause::Interrupted),
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
    driver.send(Event::Interrupt { turn: turn(1) });
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
    driver.send(Event::Interrupt { turn: turn(1) });
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
            result(0, Exit::Err(Cause::Interrupted), None),
            result(1, Exit::Err(Cause::Interrupted), None),
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
    let (_, effects) = driver.send(Event::Interrupt { turn: turn(1) });
    assert_eq!(
        effects,
        vec![Effect::CancelTool { id: read }],
        "the running tool is asked to stop"
    );
    assert_eq!(
        driver.since(from),
        vec![
            Record::CancelRequested { turn: turn(1) },
            result(1, Exit::Err(Cause::Interrupted), None)
        ],
        "the unapproved call never runs"
    );
    let (outcome, _) = driver.approve("edit", Verdict::Approve);
    assert_eq!(outcome, Outcome::Exists, "too late to approve");
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

#[test]
fn interrupt_during_backoff_ends_at_once() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    let (_, effects) = driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(Cause::Failed(Fault::Overloaded)),
    });
    let sleep = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Sleep { id, .. } => Some(*id),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::RunTool { .. }
            | Effect::CancelTool { .. } => None,
        })
        .expect("a backoff");
    driver.send(Event::Interrupt { turn: turn(1) });
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
    for (fault, delay) in [(Fault::Overloaded, 10), (Fault::Network, 20)] {
        let from = driver.log.len();
        let (_, effects) = driver.send(Event::ModelDone {
            id: call,
            result: ModelExit::Failed(Cause::Failed(fault)),
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
                | Effect::CancelTool { .. } => None,
            })
            .expect("a backoff");
        assert_eq!(wait, Millis::new(delay), "the schedule is data");
        let (_, effects) = driver.send(Event::Woke { id });
        assert_eq!(
            driver.log.last(),
            Some(&Record::StepStarted { step: step(0) }),
            "the same step again"
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
        result: ModelExit::Failed(Cause::Failed(Fault::Overloaded)),
    });
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Model(Cause::Failed(
            Fault::Overloaded
        ))))),
        "the budget is spent: the turn fails"
    );
}

#[test]
fn other_model_failures_do_not_retry() {
    let mut driver = Driver::new(small_limits());
    let call = driver.start();
    driver.send(Event::ModelDone {
        id: call,
        result: ModelExit::Failed(Cause::Failed(Fault::Other)),
    });
    assert_eq!(
        driver.log.last(),
        Some(&ended(TurnOutcome::Failed(Failure::Model(Cause::Failed(
            Fault::Other
        ))))),
        "failed"
    );
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn steps_per_turn_is_a_bound() {
    let mut driver = Driver::new(limits(LimitsConfig {
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
    let (_, effects) = driver.send(Event::Message {
        message: message(1),
        turn: turn(1),
        text: payload(5_000),
    });
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
        result: ModelExit::Failed(Cause::Failed(Fault::Network)),
    });
    let sleep = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Sleep { id, .. } => Some(*id),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::RunTool { .. }
            | Effect::CancelTool { .. } => None,
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
        driver.since(from).contains(&result(
            4,
            Exit::Err(Cause::Failed(Fault::Limit(Limit::Calls))),
            None
        )),
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
    let mut driver = Driver::new(limits(LimitsConfig {
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
        driver.log.contains(&result(
            1,
            Exit::Err(Cause::Failed(Fault::Limit(Limit::Calls))),
            None
        )),
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
        vec![ended(TurnOutcome::Failed(Failure::Model(Cause::Failed(
            Fault::Other
        ))))],
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
            result(0, Exit::Err(Cause::Unknown), None),
            Record::StepStarted { step: step(1) },
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
            result(0, Exit::Err(Cause::Unknown), None),
            Record::StepStarted { step: step(1) },
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
            result(1, Exit::Err(Cause::Unknown), None),
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
            Record::StepStarted { step: step(0) },
        ],
        "partial output is reset and the step runs again"
    );
    assert_eq!(model_call(&effects).epoch, Epoch(1), "new epoch");
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
    let log = vec![
        Record::TurnStarted {
            turn: turn(1),
            message: message(1),
            text: payload(1),
        },
        Record::StepStarted { step: step(0) },
        Record::StepFinished {
            step: step(0),
            calls: vec![propose("bash", Tool::Bash)],
        },
        Record::Dispatched { call: CallIx(0) },
    ];
    assert_eq!(
        Core::recover(small_limits(), &log),
        Err(Corrupt {
            seq: 3,
            violation: Violation::WrongPhase
        }),
        "dispatch without approval is refused"
    );
    let cancelled_then_completed = vec![
        Record::TurnStarted {
            turn: turn(1),
            message: message(1),
            text: payload(1),
        },
        Record::CancelRequested { turn: turn(1) },
        ended(TurnOutcome::Completed),
    ];
    assert_eq!(
        Core::recover(small_limits(), &cancelled_then_completed),
        Err(Corrupt {
            seq: 2,
            violation: Violation::WrongEnding
        }),
        "a turn the user stopped cannot end completed"
    );
}

/// The start of a log whose step proposed `calls`.
fn proposed(calls: Vec<Proposal<Abstract>>) -> Vec<Record<Abstract>> {
    vec![
        Record::TurnStarted {
            turn: turn(1),
            message: message(1),
            text: payload(1),
        },
        Record::StepStarted { step: step(0) },
        Record::StepFinished {
            step: step(0),
            calls,
        },
    ]
}

#[test]
fn a_log_repeating_a_message_is_corrupt() {
    let mut log = proposed(Vec::new());
    log.push(ended(TurnOutcome::Completed));
    log.push(Record::TurnStarted {
        turn: turn(2),
        message: message(1),
        text: payload(1),
    });
    assert_eq!(
        Core::recover(small_limits(), &log),
        Err(Corrupt {
            seq: 4,
            violation: Violation::DuplicateMessage
        }),
        "one message starts one turn"
    );
}

#[test]
fn policy_approves_only_safe_calls() {
    let mut log = proposed(vec![propose("edit", Tool::Edit)]);
    log.push(Record::Approved {
        call: CallIx(0),
        by: Approver::Policy,
    });
    assert_eq!(
        Core::recover(small_limits(), &log),
        Err(Corrupt {
            seq: 3,
            violation: Violation::WrongApprover
        }),
        "an edit needs the user"
    );
    let mut read = proposed(vec![propose("read", Tool::Read)]);
    read.push(Record::Approved {
        call: CallIx(0),
        by: Approver::Policy,
    });
    assert!(
        Core::recover(small_limits(), &read).is_ok(),
        "a read is approved by policy"
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
                by: Approver::User,
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
    type Args = Tool;
    type Reason = String;
    type Output = String;

    fn tool(args: &Tool) -> Tool {
        *args
    }
}

#[test]
fn record_json_shape() {
    let turn = TurnId::parse("01900000-0000-7000-8000-000000000001").expect("a v7");
    let records: Vec<Record<Text>> = vec![
        Record::TurnStarted {
            turn,
            message: MessageId::parse("0190a8e2-7c3b-7d4e-8f00-0123456789ab").expect("a v7"),
            text: Measured {
                body: "hi".to_owned(),
                bytes: 2,
            },
        },
        Record::StepFinished {
            step: StepId { turn, index: 0 },
            calls: vec![Proposal {
                id: call_id("toolu_01"),
                request: Request::Run { args: Tool::Edit },
            }],
        },
        Record::Approved {
            call: CallIx(0),
            by: Approver::User,
        },
        Record::ToolResult {
            call: CallIx(0),
            exit: Exit::Err(Cause::Failed(Fault::Limit(Limit::Calls))),
            output: None,
        },
        Record::StepReset {
            step: StepId { turn, index: 0 },
            reason: Reset::Retry {
                fault: Fault::Overloaded,
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
            r#"{"type":"TurnStarted","turn":"01900000-0000-7000-8000-000000000001","message":"0190a8e2-7c3b-7d4e-8f00-0123456789ab","text":{"body":"hi","bytes":2}}"#,
            r#"{"type":"StepFinished","step":{"turn":"01900000-0000-7000-8000-000000000001","index":0},"calls":[{"id":"toolu_01","request":{"Run":{"args":"Edit"}}}]}"#,
            r#"{"type":"Approved","call":0,"by":"User"}"#,
            r#"{"type":"ToolResult","call":0,"exit":{"Err":{"Failed":{"Limit":"Calls"}}},"output":null}"#,
            r#"{"type":"StepReset","step":{"turn":"01900000-0000-7000-8000-000000000001","index":0},"reason":{"Retry":{"fault":"Overloaded","delay":500}}}"#,
            r#"{"type":"TurnEnded","turn":"01900000-0000-7000-8000-000000000001","outcome":{"Failed":{"Limit":"Context"}}}"#,
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
            r#"{"type":"TurnEnded","turn":"01900000-0000-4000-8000-000000000001","outcome":"Completed"}"#
        )
        .is_err(),
        "a turn id that is not a UUIDv7 does not parse"
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
                Effect::CancelModel { .. } | Effect::CancelTool { .. } => {}
            }
        }
    }
}

const fn cause(number: u8) -> Cause {
    match number {
        0 => Cause::Failed(Fault::Overloaded),
        1 => Cause::Failed(Fault::Network),
        2 => Cause::Interrupted,
        _ => Cause::Failed(Fault::Other),
    }
}

fn event_for(choice: &Choice, seen: &Seen, driver: &Driver) -> Option<Event<Abstract>> {
    Some(match choice {
        Choice::Message(number) => Event::Message {
            message: message(u64::from(*number)),
            turn: turn(u64::from(*number)),
            text: payload(u64::from(*number) * 100),
        },
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
            exit: if *ok {
                Exit::Ok
            } else {
                Exit::Err(Cause::Failed(Fault::Other))
            },
            output: payload(11),
        },
        Choice::Woke(index) => Event::Woke {
            id: pick(&seen.sleeps, *index)?,
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
