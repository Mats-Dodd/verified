//! Actors in the core: the mailbox and its consumption, steering, tasks
//! and their notifications, the cancel primitive on tasks, stops, `Keyed`
//! reruns and the limits, driven the way the shell drives the core.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use proptest::prelude::*;
use verified_core::{
    Core, Corrupt, Decision, Effect, Event, ModelExit, Outcome, Rejection, Summary, Verdict,
    Violation,
};
use verified_domain::{
    Abstract, ActorId, ActorName, Address, Body, Born, CallId, CallIx, EffectId, Envelope,
    EnvelopeId, Epoch, Exit, Grant, Limits, LimitsConfig, Measured, MessageId, Millis, ModelFault,
    Proposal, Record, Request, Rules, Shape, StepId, TaskId, Tool, TurnId, TurnOutcome, Unit,
    UnixMillis, Unstarted,
};

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

fn config() -> LimitsConfig {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    LimitsConfig {
        turns_per_actor: count(8),
        messages_per_actor: count(8),
        tasks_per_actor: count(2),
        children_per_actor: count(2),
        depth: 1,
        steps_per_turn: NonZeroU32::new(8).expect("non-zero"),
        calls_per_step: count(4),
        calls_per_turn: count(16),
        context_bytes: NonZeroU64::new(10_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    }
}

fn limits() -> Limits {
    Limits::new(config())
}

const fn payload(bytes: u64) -> Measured<Unit> {
    Measured { body: Unit, bytes }
}

fn at(number: u64) -> UnixMillis {
    UnixMillis::new(1_700_000_000_000u64.saturating_add(number)).expect("a 48-bit time")
}

fn actor() -> ActorId {
    ActorId::from_parts(at(0), &[4; 10])
}

fn child() -> ActorId {
    ActorId::from_parts(at(1), &[5; 10])
}

fn peer() -> ActorId {
    ActorId::from_parts(at(2), &[6; 10])
}

fn born(depth: u32) -> Born {
    Born {
        depth,
        ..Born::root(
            actor(),
            ActorName::parse("main").expect("a name"),
            vec![Tool::Read, Tool::Edit, Tool::Bash],
        )
    }
}

fn message(number: u64) -> MessageId {
    MessageId::from_parts(at(number), &[2; 10])
}

/// The user's message `number`.
fn said(number: u64) -> EnvelopeId {
    EnvelopeId::User(message(number))
}

fn user(number: u64) -> Event<Abstract> {
    Event::Deliver {
        envelope: Envelope {
            id: said(number),
            from: Address::User,
            to: actor(),
            reply_to: None,
            body: Body::Text(payload(10)),
        },
    }
}

/// Another actor's message, keyed by the record in its log that sent it.
fn agent(seq: u64) -> Event<Abstract> {
    Event::Deliver {
        envelope: Envelope {
            id: EnvelopeId::Minted { actor: peer(), seq },
            from: Address::Actor(peer()),
            to: actor(),
            reply_to: Some(Address::Actor(peer())),
            body: Body::Text(payload(10)),
        },
    }
}

fn stop(number: u64) -> Event<Abstract> {
    Event::Deliver {
        envelope: Envelope {
            id: said(number),
            from: Address::User,
            to: actor(),
            reply_to: None,
            body: Body::Stop,
        },
    }
}

fn call(name: &str, shape: Shape) -> Proposal<Abstract> {
    Proposal {
        id: CallId::parse(name).expect("a protocol id"),
        request: Request::Run { args: shape },
    }
}

fn read(name: &str) -> Proposal<Abstract> {
    call(
        name,
        Shape::Read {
            subject: Vec::new(),
        },
    )
}

fn edit(name: &str) -> Proposal<Abstract> {
    call(
        name,
        Shape::Edit {
            subject: Vec::new(),
        },
    )
}

/// A `bash` in the background: a task.
fn background(name: &str) -> Proposal<Abstract> {
    call(
        name,
        Shape::Bash {
            subject: Vec::new(),
            background: true,
        },
    )
}

#[derive(Debug)]
struct Driver {
    limits: Limits,
    core: Core<Abstract>,
    log: Vec<Record<Abstract>>,
}

impl Driver {
    /// An actor at `depth`, booted as the shell boots it: rules, birth.
    fn at_depth(depth: u32) -> Self {
        let mut driver = Self {
            core: Core::new(limits()),
            limits: limits(),
            log: Vec::new(),
        };
        let boot = vec![
            Record::RulesLoaded {
                rules: Rules::default(),
                file: None,
            },
            Record::Born { born: born(depth) },
        ];
        for record in boot {
            driver.core.apply(&record).expect("the boot applies");
            driver.log.push(record);
        }
        driver
    }

    fn new() -> Self {
        Self::at_depth(0)
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

    fn crash(&mut self) -> Vec<Effect<Abstract>> {
        self.core = Core::recover(self.limits.clone(), &self.log).expect("the log recovers");
        let decision = self.core.resume(Some(born(0)), None);
        self.commit(decision).1
    }

    fn since(&self, from: usize) -> Vec<Record<Abstract>> {
        self.log.get(from..).unwrap_or_default().to_vec()
    }

    fn seq(&self) -> u64 {
        u64::try_from(self.log.len()).expect("a seq")
    }

    /// Message 1 starts a turn; its first model call's id.
    fn start(&mut self) -> EffectId {
        let (outcome, effects) = self.send(user(1));
        assert_eq!(outcome, Outcome::Accepted, "the message starts a turn");
        model_call(&effects)
    }

    fn finish(&mut self, id: EffectId, calls: Vec<Proposal<Abstract>>) -> Vec<Effect<Abstract>> {
        self.send(Event::ModelDone {
            id,
            result: ModelExit::Finished(calls),
        })
        .1
    }

    fn approve(&mut self, name: &str) -> Vec<Effect<Abstract>> {
        let (outcome, effects) = self.send(Event::Approve {
            call: CallId::parse(name).expect("a protocol id"),
            verdict: Verdict::Approve,
        });
        assert_eq!(outcome, Outcome::Accepted, "{name} awaited approval");
        effects
    }

    fn done(&mut self, id: EffectId, exit: Exit) -> (Outcome, Vec<Effect<Abstract>>) {
        self.send(Event::ToolDone {
            id,
            exit,
            output: payload(3),
        })
    }

    /// The `StepStarted` records of the log, with what each carried.
    fn carried(&self) -> Vec<Vec<EnvelopeId>> {
        self.log
            .iter()
            .filter_map(|record| match record {
                Record::StepStarted { carries, .. } => Some(carries.clone()),
                Record::Born { .. }
                | Record::RulesLoaded { .. }
                | Record::Delivered { .. }
                | Record::TurnStarted { .. }
                | Record::ModelOutput { .. }
                | Record::StepReset { .. }
                | Record::StepFinished { .. }
                | Record::Approved { .. }
                | Record::Dispatched { .. }
                | Record::ToolResult { .. }
                | Record::TaskEnded { .. }
                | Record::StopSent { .. }
                | Record::CancelRequested { .. }
                | Record::TurnEnded { .. }
                | Record::EpochBumped { .. }
                | Record::CompactionStarted { .. }
                | Record::Compacted { .. } => None,
            })
            .filter(|carries| !carries.is_empty())
            .collect()
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
            | Effect::Stop { .. }
            | Effect::Summarize { .. } => None,
        })
        .expect("a CallModel effect")
}

/// The tool runs started, as (effect id, call id, key).
fn runs(effects: &[Effect<Abstract>]) -> Vec<(EffectId, CallId, u64)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::RunTool { call } => Some((call.effect(), call.id().clone(), call.key())),
            Effect::CallModel { .. }
            | Effect::CancelModel { .. }
            | Effect::AskApproval { .. }
            | Effect::CancelTool { .. }
            | Effect::Sleep { .. }
            | Effect::Stop { .. }
            | Effect::Summarize { .. } => None,
        })
        .collect()
}

/// The run of call `name` among `effects`.
fn run_of(effects: &[Effect<Abstract>], name: &str) -> EffectId {
    runs(effects)
        .into_iter()
        .find_map(|(effect, id, _)| (id.as_str() == name).then_some(effect))
        .expect("the call runs")
}

fn has_model_call(effects: &[Effect<Abstract>]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::CallModel { .. }))
}

/// A turn whose model proposed `calls`, each approved in turn (all but
/// reads ask), and every effect that started.
fn proposing(driver: &mut Driver, calls: Vec<Proposal<Abstract>>) -> Vec<Effect<Abstract>> {
    let call = driver.start();
    let names: Vec<CallId> = calls.iter().map(|proposal| proposal.id.clone()).collect();
    let mut effects = driver.finish(call, calls);
    for name in names {
        let (_, approved) = driver.send(Event::Approve {
            call: name,
            verdict: Verdict::Approve,
        });
        effects.extend(approved);
    }
    effects
}

// ---------------------------------------------------------------------------
// Tasks (P1, T3, T4)
// ---------------------------------------------------------------------------

/// P1, T4: a background call resolves `Ok` in the decision that dispatches
/// it, so the turn goes on without waiting; the task's end is one
/// `TaskEnded`, its notification, and it starts the next turn.
#[test]
fn a_background_call_resolves_at_its_start_and_its_end_notifies_once() {
    let mut driver = Driver::new();
    let call = driver.start();
    driver.finish(call, vec![background("bg")]);
    let from = driver.log.len();
    let dispatched = driver.seq().saturating_add(1);
    let effects = driver.approve("bg");
    assert_eq!(
        driver.since(from),
        vec![
            Record::Approved {
                call: CallIx(0),
                by: Grant::User
            },
            Record::Dispatched { call: CallIx(0) },
            Record::ToolResult {
                call: CallIx(0),
                exit: Exit::Ok,
                output: None
            },
            Record::StepStarted {
                step: StepId {
                    turn: TurnId(3),
                    index: 1
                },
                carries: Vec::new()
            },
        ],
        "the call resolves at its start, and the next step starts at once"
    );
    let task = run_of(&effects, "bg");
    assert_eq!(task.seq, dispatched, "the task is its Dispatched's seq");
    driver.finish(model_call(&effects), Vec::new());
    assert_eq!(
        driver.core.running(),
        None,
        "the turn ended while the task runs"
    );
    let from = driver.log.len();
    let (_, effects) = driver.done(task, Exit::Ok);
    let ended = driver.seq();
    assert_eq!(
        driver.since(from).first(),
        Some(&Record::TaskEnded {
            task: TaskId(task.seq),
            exit: Exit::Ok,
            output: Some(payload(3))
        }),
        "the task ends with its report"
    );
    assert!(has_model_call(&effects), "the notification starts a turn");
    assert_eq!(
        driver.carried().last(),
        Some(&vec![EnvelopeId::Task(TaskId(task.seq))]),
        "whose model call carries it (ended at {ended})"
    );
    let (outcome, _) = driver.done(task, Exit::Ok);
    assert_eq!(outcome, Outcome::Stale, "a task ends once");
}

/// T3: of a task's report and its deadline, the first wins; the deadline
/// is v cancelling: `Unknown`, and the run is asked to stop.
#[test]
fn a_deadline_ends_a_task_unknown() {
    let mut driver = Driver::new();
    let effects = proposing(&mut driver, vec![background("bg")]);
    let task = run_of(&effects, "bg");
    let from = driver.log.len();
    let (outcome, effects) = driver.send(Event::Expired { id: task });
    assert_eq!(outcome, Outcome::Accepted, "the deadline is live");
    assert!(
        effects.contains(&Effect::CancelTool { id: task }),
        "signalled once: {effects:?}"
    );
    assert_eq!(
        driver.since(from).first(),
        Some(&Record::TaskEnded {
            task: TaskId(task.seq),
            exit: Exit::Unknown,
            output: None
        }),
        "it may have acted"
    );
    assert_eq!(driver.done(task, Exit::Ok).0, Outcome::Stale, "too late");
}

/// The cancel primitive on tasks: a timer that has not fired never acts,
/// a running command is signalled once, a stop is not cancelled, and the
/// `cancel` call says whether anything was.
#[test]
fn cancel_revokes_a_task() {
    let mut driver = Driver::new();
    let effects = proposing(
        &mut driver,
        vec![call("wake", Shape::Wake), background("bg")],
    );
    let wake = run_of(&effects, "wake");
    let bash = run_of(&effects, "bg");
    let next = driver.finish(
        model_call(&effects),
        vec![
            call(
                "cancel-wake",
                Shape::Cancel {
                    task: TaskId(wake.seq),
                },
            ),
            call(
                "cancel-bash",
                Shape::Cancel {
                    task: TaskId(bash.seq),
                },
            ),
            call(
                "cancel-none",
                Shape::Cancel {
                    task: TaskId(9_999),
                },
            ),
        ],
    );
    assert!(runs(&next).is_empty(), "a cancel asks first");
    driver.approve("cancel-wake");
    let from = driver.log.len();
    let effects = driver.approve("cancel-bash");
    assert!(
        effects.contains(&Effect::CancelTool { id: bash }),
        "the command is signalled: {effects:?}"
    );
    assert!(
        driver.since(from).contains(&Record::ToolResult {
            call: CallIx(3),
            exit: Exit::Ok,
            output: None
        }),
        "something was cancelled"
    );
    driver.approve("cancel-none");
    assert!(
        driver.log.contains(&Record::TaskEnded {
            task: TaskId(wake.seq),
            exit: Exit::NotStarted(Unstarted::Cancelled),
            output: None
        }),
        "the timer never fires"
    );
    assert!(
        driver.log.contains(&Record::ToolResult {
            call: CallIx(4),
            exit: Exit::Failed,
            output: None
        }),
        "no task by that id: nothing was cancelled"
    );
    let (_, _) = driver.done(bash, Exit::Unknown);
    assert!(
        driver.log.contains(&Record::TaskEnded {
            task: TaskId(bash.seq),
            exit: Exit::Unknown,
            output: Some(payload(3))
        }),
        "the command reports what came back"
    );
}

/// C3: a running task is signalled once: a repeated `cancel` of it, a stop
/// or its deadline after the cancel signal nothing more.
#[test]
fn a_task_is_signalled_once() {
    let mut driver = Driver::new();
    let effects = proposing(&mut driver, vec![background("bg")]);
    let bash = run_of(&effects, "bg");
    let cancel = |name: &str| {
        call(
            name,
            Shape::Cancel {
                task: TaskId(bash.seq),
            },
        )
    };
    driver.finish(
        model_call(&effects),
        vec![cancel("first"), cancel("second")],
    );
    let signal = Effect::CancelTool { id: bash };
    assert!(
        driver.approve("first").contains(&signal),
        "the first cancel signals the command"
    );
    let effects = driver.approve("second");
    assert!(
        !effects.contains(&signal),
        "a repeated cancel signals nothing: {effects:?}"
    );
    assert!(
        driver.log.contains(&Record::ToolResult {
            call: CallIx(2),
            exit: Exit::Ok,
            output: None
        }),
        "the command is being cancelled"
    );
    let (_, effects) = driver.send(stop(9));
    assert!(!effects.contains(&signal), "nor does a stop: {effects:?}");
    let (_, effects) = driver.send(Event::Expired { id: bash });
    assert!(!effects.contains(&signal), "nor its deadline: {effects:?}");
}

/// C3: a call is signalled once: its deadline after Esc signalled it
/// records `Unknown` and signals nothing more.
#[test]
fn a_deadline_after_esc_signals_nothing_more() {
    let mut driver = Driver::new();
    let call = driver.start();
    let effects = driver.finish(call, vec![read("read")]);
    let read = run_of(&effects, "read");
    let (_, effects) = driver.send(Event::Interrupt { turn: TurnId(3) });
    let signal = Effect::CancelTool { id: read };
    assert!(effects.contains(&signal), "Esc signals the read");
    let (outcome, effects) = driver.send(Event::Expired { id: read });
    assert_eq!(outcome, Outcome::Accepted, "the deadline is live");
    assert!(!effects.contains(&signal), "signalled once: {effects:?}");
    assert!(
        driver.log.contains(&Record::ToolResult {
            call: CallIx(0),
            exit: Exit::Unknown,
            output: None
        }),
        "it may have acted"
    );
}

/// C2, T3: after a crash a running `Unsafe` task ends `Unknown` and never
/// runs again; a `Safe` one (a timer) runs again under the same seq.
#[test]
fn a_crash_ends_an_unsafe_task_unknown_and_reruns_a_timer() {
    let mut driver = Driver::new();
    let effects = proposing(
        &mut driver,
        vec![call("wake", Shape::Wake), background("bg")],
    );
    let wake = run_of(&effects, "wake");
    let bash = run_of(&effects, "bg");
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(
        driver.since(from).contains(&Record::TaskEnded {
            task: TaskId(bash.seq),
            exit: Exit::Unknown,
            output: None
        }),
        "the command may have acted"
    );
    let reruns = runs(&effects);
    assert!(
        reruns.iter().any(|(id, _, _)| *id
            == EffectId {
                epoch: Epoch(1),
                seq: wake.seq
            }),
        "the timer runs again in the new epoch: {reruns:?}"
    );
    assert!(
        reruns.iter().all(|(id, _, _)| id.seq != bash.seq),
        "the command never runs again"
    );
}

/// Limits: a background call past `tasks_per_actor` does not start.
#[test]
fn tasks_are_bounded() {
    let mut driver = Driver::new();
    proposing(
        &mut driver,
        vec![background("a"), background("b"), background("c")],
    );
    assert!(
        driver.log.contains(&Record::ToolResult {
            call: CallIx(2),
            exit: Exit::NotStarted(Unstarted::Limit),
            output: None
        }),
        "the third task is past the bound"
    );
}

// ---------------------------------------------------------------------------
// The mailbox (T4) and steering
// ---------------------------------------------------------------------------

/// T4: notifications that arrive together are carried by one model call;
/// a user message is carried alone, after them.
#[test]
fn notifications_coalesce_and_a_user_message_is_carried_alone() {
    let mut driver = Driver::new();
    let effects = proposing(&mut driver, vec![background("a"), background("b")]);
    let first = run_of(&effects, "a");
    let second = run_of(&effects, "b");
    let call = model_call(&effects);
    driver.done(first, Exit::Ok);
    driver.send(agent(40));
    driver.done(second, Exit::Failed);
    driver.send(user(2));
    driver.send(agent(41));
    assert_eq!(driver.core.unread(), 5, "everything waits for the turn");
    let effects = driver.finish(call, Vec::new());
    assert_eq!(
        driver.carried().last(),
        Some(&vec![
            EnvelopeId::Task(TaskId(first.seq)),
            EnvelopeId::Minted {
                actor: peer(),
                seq: 40
            },
            EnvelopeId::Task(TaskId(second.seq)),
        ]),
        "the run before the user message, together"
    );
    let effects = driver.finish(model_call(&effects), Vec::new());
    assert_eq!(
        driver.carried().last(),
        Some(&vec![said(2)]),
        "the user's message alone"
    );
    driver.finish(model_call(&effects), Vec::new());
    assert_eq!(
        driver.carried().last(),
        Some(&vec![EnvelopeId::Minted {
            actor: peer(),
            seq: 41
        }]),
        "then the rest"
    );
    assert_eq!(driver.core.unread(), 0, "each consumed once");
}

/// T4 and steering: a user message while the step waits for approvals
/// denies them ("the user steered"), and lands at the step's boundary,
/// after every result of the step.
#[test]
fn a_user_message_denies_waiting_approvals() {
    let mut driver = Driver::new();
    let call = driver.start();
    let effects = driver.finish(call, vec![read("read"), edit("edit")]);
    let read = run_of(&effects, "read");
    let from = driver.log.len();
    let (outcome, effects) = driver.send(user(2));
    assert_eq!(outcome, Outcome::Accepted, "steering is accepted");
    assert!(!has_model_call(&effects), "the read is still running");
    assert_eq!(
        driver.since(from),
        vec![
            Record::Delivered {
                envelope: Envelope {
                    id: said(2),
                    from: Address::User,
                    to: actor(),
                    reply_to: None,
                    body: Body::Text(payload(10)),
                }
            },
            Record::ToolResult {
                call: CallIx(1),
                exit: Exit::NotStarted(Unstarted::Steered),
                output: None
            },
        ],
        "the edit awaiting approval is denied: the user steered"
    );
    let from = driver.log.len();
    let (_, effects) = driver.done(read, Exit::Ok);
    assert!(has_model_call(&effects), "the next step starts");
    assert_eq!(
        driver.since(from),
        vec![
            Record::ToolResult {
                call: CallIx(0),
                exit: Exit::Ok,
                output: Some(payload(3))
            },
            Record::StepStarted {
                step: StepId {
                    turn: TurnId(3),
                    index: 1
                },
                carries: vec![said(2)]
            },
        ],
        "the message lands after every result of the step"
    );
}

/// Steering survives a crash: one torn after the message's `Delivered` is
/// finished on resume, so the approval it denies is not asked again.
#[test]
fn a_torn_steer_is_finished_on_resume() {
    let mut driver = Driver::new();
    let call = driver.start();
    driver.finish(call, vec![edit("edit")]);
    let decision = driver.core.decide(user(2));
    let delivered = decision
        .records
        .first()
        .cloned()
        .expect("the message's record");
    driver.core.apply(&delivered).expect("applies");
    driver.log.push(delivered);
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(
        driver.since(from).contains(&Record::ToolResult {
            call: CallIx(0),
            exit: Exit::NotStarted(Unstarted::Steered),
            output: None
        }),
        "the edit is denied: the user steered"
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::AskApproval { .. })),
        "and not asked again: {effects:?}"
    );
    assert!(has_model_call(&effects), "the message lands at once");
}

/// Steering: a user message that arrives during the model call denies the
/// approvals its step then waits for, as one arriving during the wait does.
#[test]
fn a_message_during_the_model_call_steers_its_step() {
    let mut driver = Driver::new();
    let call = driver.start();
    driver.send(user(2));
    let from = driver.log.len();
    let effects = driver.finish(call, vec![edit("edit")]);
    assert!(
        driver.since(from).contains(&Record::ToolResult {
            call: CallIx(0),
            exit: Exit::NotStarted(Unstarted::Steered),
            output: None
        }),
        "the edit is denied: the user steered"
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::AskApproval { .. })),
        "nothing waits on the user: {effects:?}"
    );
    assert!(has_model_call(&effects), "and the message is carried next");
}

/// T4: an envelope is consumed when a model call carrying it starts, not
/// when it arrives: one that arrived and was never carried (the turn was
/// cancelled first) starts the next turn.
#[test]
fn an_unseen_message_starts_the_next_turn() {
    let mut driver = Driver::new();
    let call = driver.start();
    let effects = driver.finish(call, vec![read("read")]);
    let read = run_of(&effects, "read");
    driver.send(user(2));
    driver.send(Event::Interrupt { turn: TurnId(3) });
    let from = driver.log.len();
    let (_, effects) = driver.done(read, Exit::Ok);
    let next = TurnId(driver.seq().saturating_sub(2));
    assert_eq!(
        driver.since(from),
        vec![
            Record::ToolResult {
                call: CallIx(0),
                exit: Exit::Ok,
                output: Some(payload(3))
            },
            Record::TurnEnded {
                turn: TurnId(3),
                outcome: TurnOutcome::Cancelled
            },
            Record::TurnStarted { turn: next },
            Record::StepStarted {
                step: StepId {
                    turn: next,
                    index: 0
                },
                carries: vec![said(2)]
            },
        ],
        "Esc ends the turn; the message it never saw starts the next"
    );
    assert!(has_model_call(&effects), "and is carried");
}

/// A turn whose step is retried carries nothing new: a retry repeats the
/// same model call.
#[test]
fn a_retry_carries_nothing_new() {
    let mut driver = Driver::new();
    let call = driver.start();
    driver.send(user(2));
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
            | Effect::Stop { .. }
            | Effect::Summarize { .. } => None,
        })
        .expect("a backoff");
    driver.send(Event::Woke { id: sleep });
    assert_eq!(
        driver.carried(),
        vec![vec![said(1)]],
        "the waiting message is not carried by the retry"
    );
    assert_eq!(driver.core.unread(), 1, "it waits for the next turn");
}

// ---------------------------------------------------------------------------
// Stops (C3) and the stop cascade
// ---------------------------------------------------------------------------

/// C3: a stop revokes everything the actor holds: the turn is cancelled,
/// what awaits approval never starts, a timer never fires, a running
/// command is signalled, every child is sent a stop; and nothing starts
/// after it.
#[test]
fn a_stop_revokes_everything_and_stops_every_child() {
    let mut driver = Driver::new();
    let spawn = call(
        "spawn",
        Shape::Spawn {
            subject: b"helper".to_vec(),
            child: child(),
        },
    );
    let effects = proposing(
        &mut driver,
        vec![call("wake", Shape::Wake), background("bg"), spawn],
    );
    let wake = run_of(&effects, "wake");
    let bash = run_of(&effects, "bg");
    let spawned = run_of(&effects, "spawn");
    let from = driver.log.len();
    let (outcome, effects) = driver.send(stop(9));
    assert_eq!(outcome, Outcome::Accepted, "the stop lands");
    let records = driver.since(from);
    let stop_sent = driver
        .log
        .iter()
        .position(|record| matches!(record, Record::StopSent { .. }))
        .expect("a StopSent");
    assert!(
        records.contains(&Record::TaskEnded {
            task: TaskId(wake.seq),
            exit: Exit::NotStarted(Unstarted::Stopped),
            output: None
        }),
        "the timer never fires: {records:?}"
    );
    assert!(
        records.contains(&Record::StopSent { child: child() }),
        "the child is stopped"
    );
    assert!(
        effects.contains(&Effect::CancelTool { id: bash }),
        "the command is signalled"
    );
    assert!(
        effects.contains(&Effect::Stop {
            id: EffectId {
                epoch: Epoch(0),
                seq: u64::try_from(stop_sent).expect("a seq")
            },
            child: child()
        }),
        "the stop is delivered, keyed by its StopSent: {effects:?}"
    );
    assert!(runs(&effects).is_empty(), "nothing starts");
    assert_eq!(
        driver.send(user(10)).0,
        Outcome::Rejected(Rejection::Stopped),
        "a stopped actor takes no more mail"
    );
    assert_eq!(driver.send(stop(9)).0, Outcome::Exists, "a repeat stop");
    let (_, effects) = driver.done(spawned, Exit::Ok);
    assert!(runs(&effects).is_empty(), "nothing starts after the stop");
    assert!(!has_model_call(&effects), "no model call either");
    assert!(
        driver.log.iter().all(|record| !matches!(
            record,
            Record::TurnStarted { turn } if turn.0 > u64::try_from(from).expect("a seq")
        )),
        "and no turn"
    );
}

/// C3: a crash that tears a stop's group (the `Delivered` durable, the
/// rest not) is finished on resume: the turn is cancelled and every child
/// stopped.
#[test]
fn a_torn_stop_is_finished_on_resume() {
    let mut driver = Driver::new();
    let spawn = call(
        "spawn",
        Shape::Spawn {
            subject: b"helper".to_vec(),
            child: child(),
        },
    );
    proposing(&mut driver, vec![spawn]);
    let decision = driver.core.decide(stop(9));
    let delivered = decision
        .records
        .first()
        .cloned()
        .expect("the stop's record");
    driver.core.apply(&delivered).expect("applies");
    driver.log.push(delivered);
    let from = driver.log.len();
    let effects = driver.crash();
    let records = driver.since(from);
    assert!(
        records.contains(&Record::CancelRequested { turn: TurnId(3) }),
        "the turn is cancelled: {records:?}"
    );
    assert!(
        records.contains(&Record::StopSent { child: child() }),
        "the child is stopped"
    );
    assert!(
        effects.iter().any(
            |effect| matches!(effect, Effect::Stop { child: stopped, .. } if *stopped == child())
        ),
        "and its stop delivered"
    );
    assert!(runs(&effects).is_empty(), "nothing reruns under a stop");
}

/// C3: a stop lands whatever the actor holds: the message limit bounds
/// mail, never the stop that ends a runaway actor.
#[test]
fn a_stop_lands_at_the_message_limit() {
    let mut driver = Driver::new();
    for seq in 1..=8u64 {
        assert_eq!(
            driver.send(agent(seq)).0,
            Outcome::Accepted,
            "message {seq}"
        );
    }
    assert_eq!(
        driver.send(agent(9)).0,
        Outcome::Rejected(Rejection::Limit),
        "mail past the limit is refused"
    );
    assert_eq!(
        driver.send(stop(100)).0,
        Outcome::Accepted,
        "the stop lands"
    );
    assert!(driver.core.stopped(), "and the actor is stopped");
}

/// C3: after a crash a stopped actor starts nothing of its own again: a
/// timer it held ends `NotStarted` and never reruns.
#[test]
fn a_crash_after_a_stop_reruns_no_timer() {
    let mut driver = Driver::new();
    let effects = proposing(&mut driver, vec![call("wake", Shape::Wake)]);
    let wake = run_of(&effects, "wake");
    let decision = driver.core.decide(stop(9));
    let delivered = decision
        .records
        .first()
        .cloned()
        .expect("the stop's record");
    driver.core.apply(&delivered).expect("applies");
    driver.log.push(delivered);
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(
        driver.since(from).contains(&Record::TaskEnded {
            task: TaskId(wake.seq),
            exit: Exit::NotStarted(Unstarted::Stopped),
            output: None
        }),
        "the timer never fires"
    );
    assert!(runs(&effects).is_empty(), "nothing reruns: {effects:?}");
}

/// Limits: a spawn past `depth` does not start.
#[test]
fn spawns_are_bounded_by_depth() {
    let mut driver = Driver::at_depth(1);
    let call = driver.start();
    driver.finish(call, vec![call_spawn()]);
    assert!(
        driver.log.contains(&Record::ToolResult {
            call: CallIx(0),
            exit: Exit::NotStarted(Unstarted::Limit),
            output: None
        }),
        "an actor at the deepest depth spawns nothing"
    );
}

/// Limits: a `spawn` past `children_per_actor` does not start, so what a
/// compaction lists as open is bounded.
#[test]
fn spawns_are_bounded_by_children_per_actor() {
    let mut driver = Driver::new();
    let spawn = |name: &str, number: u64| {
        call(
            name,
            Shape::Spawn {
                subject: b"helper".to_vec(),
                child: ActorId::from_parts(at(10 + number), &[7; 10]),
            },
        )
    };
    proposing(
        &mut driver,
        vec![spawn("a", 1), spawn("b", 2), spawn("c", 3)],
    );
    assert!(
        driver.log.contains(&Record::ToolResult {
            call: CallIx(2),
            exit: Exit::NotStarted(Unstarted::Limit),
            output: None
        }),
        "the third child is past the bound"
    );
}

/// A refused spawn made no child: a compaction's open work lists only the
/// children a spawn made.
#[test]
fn open_work_lists_no_child_of_a_refused_spawn() {
    let mut driver = Driver::new();
    let born = ActorId::from_parts(at(12), &[7; 10]);
    let spawn = |name: &str, child: ActorId| {
        call(
            name,
            Shape::Spawn {
                subject: b"helper".to_vec(),
                child,
            },
        )
    };
    let effects = proposing(&mut driver, vec![spawn("a", child()), spawn("b", born)]);
    driver.done(run_of(&effects, "a"), Exit::NotStarted(Unstarted::Refused));
    let (_, effects) = driver.done(run_of(&effects, "b"), Exit::Ok);
    driver.finish(model_call(&effects), Vec::new());
    let (_, effects) = driver.send(Event::Compact);
    let id = effects
        .iter()
        .find_map(|effect| {
            if let Effect::Summarize { id } = effect {
                Some(*id)
            } else {
                None
            }
        })
        .expect("a summarize call");
    driver.send(Event::Summarized {
        id,
        summary: Summary::Written(payload(10)),
    });
    let children = driver.log.last().and_then(|record| {
        if let Record::Compacted { open, .. } = record {
            Some(open.children.clone())
        } else {
            None
        }
    });
    assert_eq!(children, Some(vec![born]), "only the child a spawn made");
}

fn call_spawn() -> Proposal<Abstract> {
    call(
        "spawn",
        Shape::Spawn {
            subject: b"helper".to_vec(),
            child: child(),
        },
    )
}

// ---------------------------------------------------------------------------
// Keyed calls (C2)
// ---------------------------------------------------------------------------

/// C2: a `Keyed` call dispatched before a crash runs again with the same
/// key (the seq of its `Dispatched`), so its target can accept it once.
#[test]
fn a_keyed_call_reruns_with_its_key() {
    let mut driver = Driver::new();
    let send = call(
        "send",
        Shape::Send {
            subject: b"peer".to_vec(),
        },
    );
    let effects = proposing(&mut driver, vec![send]);
    let (before, _, key) = runs(&effects).into_iter().next().expect("the send runs");
    assert_eq!(key, before.seq, "its key is its Dispatched's seq");
    let effects = driver.crash();
    let (after, _, rekey) = runs(&effects).into_iter().next().expect("it runs again");
    assert_eq!(after.epoch, Epoch(1), "in the new epoch");
    assert_eq!(rekey, key, "under the same key");
}

/// C2 and C3: a `Keyed` call whose turn was cancelled before a crash is
/// not sent again: it resolves `Unknown` (it may have taken effect once).
#[test]
fn a_keyed_call_cancelled_before_a_crash_resolves_unknown() {
    let mut driver = Driver::new();
    let send = call(
        "send",
        Shape::Send {
            subject: b"peer".to_vec(),
        },
    );
    proposing(&mut driver, vec![send]);
    driver.send(Event::Interrupt { turn: TurnId(3) });
    let from = driver.log.len();
    let effects = driver.crash();
    assert!(
        driver.since(from).contains(&Record::ToolResult {
            call: CallIx(0),
            exit: Exit::Unknown,
            output: None
        }),
        "it may have been delivered"
    );
    assert!(
        runs(&effects).is_empty(),
        "and is not sent again: {effects:?}"
    );
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

/// State is the replay: a log that consumes out of order, delivers to a stopped actor,
/// ends a task twice or stops a stranger does not replay.
#[test]
fn a_log_breaking_the_mailbox_or_the_tasks_is_corrupt() {
    let mut driver = Driver::new();
    let effects = proposing(&mut driver, vec![background("bg")]);
    let task = run_of(&effects, "bg");
    driver.done(task, Exit::Ok);
    let log = driver.log.clone();
    let mut twice = log.clone();
    twice.push(Record::TaskEnded {
        task: TaskId(task.seq),
        exit: Exit::Ok,
        output: Some(payload(1)),
    });
    assert_eq!(
        Core::recover(limits(), &twice).map(|_| ()),
        Err(Corrupt {
            seq: u64::try_from(log.len()).expect("a seq"),
            violation: Violation::UnknownTask
        }),
        "a task ends once"
    );
    let mut stranger = log.clone();
    let stopped = Driver::new();
    let decision = stopped.core.decide(stop(9));
    stranger.truncate(2);
    stranger.extend(decision.records);
    stranger.push(Record::StopSent { child: child() });
    assert_eq!(
        Core::recover(limits(), &stranger).map(|_| ()),
        Err(Corrupt {
            seq: 3,
            violation: Violation::WrongStop
        }),
        "only a spawned child is stopped"
    );
    let mut late = stranger;
    late.truncate(3);
    late.push(Record::Delivered {
        envelope: Envelope {
            id: said(10),
            from: Address::User,
            to: actor(),
            reply_to: None,
            body: Body::Text(payload(1)),
        },
    });
    assert_eq!(
        Core::recover(limits(), &late).map(|_| ()),
        Err(Corrupt {
            seq: 3,
            violation: Violation::Stopped
        }),
        "a stopped actor takes no mail"
    );
}

/// T4: a step carries exactly the batch at the head of the mailbox.
#[test]
fn a_log_carrying_other_than_the_head_is_corrupt() {
    let mut driver = Driver::new();
    driver.send(user(1));
    let mut log: Vec<Record<Abstract>> = driver.log.get(..3).unwrap_or_default().to_vec();
    log.push(Record::TurnStarted { turn: TurnId(3) });
    log.push(Record::StepStarted {
        step: StepId {
            turn: TurnId(3),
            index: 0,
        },
        carries: vec![said(2)],
    });
    assert_eq!(
        Core::recover(limits(), &log).map(|_| ()),
        Err(Corrupt {
            seq: 4,
            violation: Violation::WrongCarry
        }),
        "a message never delivered is carried"
    );
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// An abstract input, resolved against whatever is outstanding.
#[derive(Clone, Debug)]
enum Choice {
    User(u8),
    Agent(u8),
    Stop,
    Approve(u8, bool),
    Interrupt,
    Finish(u8, u8),
    Fail(u8),
    Done(u8, u8),
    Expire(u8),
    Woke(u8),
    /// The user's `/compact`.
    Compact,
    /// A summarize call answers: a summary, or a failure.
    Summarized(u8, bool),
    Crash,
}

fn choice() -> impl Strategy<Value = Choice> {
    prop_oneof![
        3 => (0u8..4).prop_map(Choice::User),
        2 => (0u8..4).prop_map(Choice::Agent),
        1 => Just(Choice::Stop),
        3 => (any::<u8>(), any::<bool>()).prop_map(|(pick, yes)| Choice::Approve(pick, yes)),
        1 => Just(Choice::Interrupt),
        4 => (any::<u8>(), 0u8..7).prop_map(|(pick, script)| Choice::Finish(pick, script)),
        1 => any::<u8>().prop_map(Choice::Fail),
        4 => (any::<u8>(), 0u8..4).prop_map(|(pick, exit)| Choice::Done(pick, exit)),
        1 => any::<u8>().prop_map(Choice::Expire),
        1 => any::<u8>().prop_map(Choice::Woke),
        1 => Just(Choice::Compact),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(pick, written)| Choice::Summarized(pick, written)),
        1 => Just(Choice::Crash),
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

/// Every id handed out, live or stale.
#[derive(Default)]
struct Seen {
    models: Vec<EffectId>,
    runs: Vec<EffectId>,
    sleeps: Vec<EffectId>,
    summaries: Vec<EffectId>,
    asks: Vec<CallId>,
    calls: u32,
}

impl Seen {
    fn note(&mut self, effects: &[Effect<Abstract>]) {
        for effect in effects {
            match effect {
                Effect::CallModel { id, .. } => self.models.push(*id),
                Effect::RunTool { call } => self.runs.push(call.effect()),
                Effect::Stop { id, .. } => self.runs.push(*id),
                Effect::Sleep { id, .. } => self.sleeps.push(*id),
                Effect::Summarize { id } => self.summaries.push(*id),
                Effect::AskApproval { request } => self.asks.push(request.id().clone()),
                Effect::CancelModel { .. } | Effect::CancelTool { .. } => {}
            }
        }
    }

    fn script(&mut self, number: u8) -> Vec<Proposal<Abstract>> {
        let shapes: Vec<Shape> = match number {
            0 => Vec::new(),
            1 => vec![Shape::Read {
                subject: Vec::new(),
            }],
            2 => vec![Shape::Bash {
                subject: Vec::new(),
                background: true,
            }],
            3 => vec![
                Shape::Wake,
                Shape::Edit {
                    subject: Vec::new(),
                },
            ],
            4 => vec![Shape::Spawn {
                subject: Vec::new(),
                child: child(),
            }],
            5 => vec![Shape::Send {
                subject: Vec::new(),
            }],
            _ => vec![Shape::Cancel {
                task: TaskId(u64::from(self.calls) % 40),
            }],
        };
        shapes
            .into_iter()
            .map(|shape| {
                self.calls = self.calls.saturating_add(1);
                call(&format!("c{}", self.calls), shape)
            })
            .collect()
    }

    fn event(&mut self, choice: &Choice, driver: &Driver) -> Option<Event<Abstract>> {
        Some(match choice {
            Choice::User(number) => user(u64::from(*number)),
            Choice::Agent(number) => agent(u64::from(*number)),
            Choice::Stop => stop(99),
            Choice::Approve(index, yes) => Event::Approve {
                call: pick(&self.asks, *index)?,
                verdict: if *yes {
                    Verdict::Approve
                } else {
                    Verdict::Deny
                },
            },
            Choice::Interrupt => Event::Interrupt {
                turn: driver.core.running()?,
            },
            Choice::Finish(index, number) => {
                let id = pick(&self.models, *index)?;
                Event::ModelDone {
                    id,
                    result: ModelExit::Finished(self.script(*number)),
                }
            }
            Choice::Fail(index) => Event::ModelDone {
                id: pick(&self.models, *index)?,
                result: ModelExit::Failed(ModelFault::Overloaded),
            },
            Choice::Done(index, exit) => Event::ToolDone {
                id: pick(&self.runs, *index)?,
                exit: match exit {
                    0 => Exit::Ok,
                    1 => Exit::Failed,
                    2 => Exit::Unknown,
                    _ => Exit::NotStarted(Unstarted::Cancelled),
                },
                output: payload(5),
            },
            Choice::Expire(index) => Event::Expired {
                id: pick(&self.runs, *index)?,
            },
            Choice::Woke(index) => Event::Woke {
                id: pick(&self.sleeps, *index)?,
            },
            Choice::Compact => Event::Compact,
            Choice::Summarized(index, written) => Event::Summarized {
                id: pick(&self.summaries, *index)?,
                summary: if *written {
                    Summary::Written(payload(7))
                } else {
                    Summary::Failed(ModelFault::Other)
                },
            },
            Choice::Crash => return None,
        })
    }
}

/// Each envelope id among `log`'s carries, and how many steps carried it.
fn carried_counts(log: &[Record<Abstract>]) -> Vec<(EnvelopeId, usize)> {
    let mut counts: Vec<(EnvelopeId, usize)> = Vec::new();
    for record in log {
        if let Record::StepStarted { carries, .. } = record {
            for id in carries {
                match counts.iter_mut().find(|(seen, _)| seen == id) {
                    Some((_, count)) => *count = count.saturating_add(1),
                    None => counts.push((*id, 1)),
                }
            }
        }
    }
    counts
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// With deliveries, steering, tasks, stops and crashes: decide never
    /// panics, apply takes every record decide made, recovery reproduces
    /// the live state, no envelope is carried twice (T4) and no task ends
    /// twice (T3).
    #[test]
    fn actors_recover_and_carry_each_envelope_once(
        choices in proptest::collection::vec(choice(), 0..80),
    ) {
        let mut driver = Driver::new();
        let mut seen = Seen::default();
        for choice in &choices {
            let effects = if matches!(choice, Choice::Crash) {
                driver.crash()
            } else if let Some(event) = seen.event(choice, &driver) {
                driver.send(event).1
            } else {
                continue;
            };
            seen.note(&effects);
            let recovered = Core::recover(limits(), &driver.log);
            prop_assert_eq!(recovered.as_ref(), Ok(&driver.core), "recover(log) == live state");
        }
        for (id, count) in carried_counts(&driver.log) {
            prop_assert_eq!(count, 1, "{:?} carried once", id);
        }
        let mut ended: Vec<TaskId> = Vec::new();
        for record in &driver.log {
            if let Record::TaskEnded { task, .. } = record {
                prop_assert!(!ended.contains(task), "{:?} ends once", task);
                ended.push(*task);
            }
        }
    }
}
