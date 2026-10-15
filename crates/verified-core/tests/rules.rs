//! Grants (C1): the matcher against a naive reference, and the core
//! granting by rule, or asking, call by call.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use proptest::prelude::*;
use verified_core::rules::grant;
use verified_core::{Core, Corrupt, Decision, Effect, Event, ModelExit, Outcome, Violation};
use verified_domain::{
    ActorId, ActorName, Address, Body, Born, CallId, CallIx, Envelope, EnvelopeId, Grant, Limits,
    LimitsConfig, Measured, MessageId, Millis, Pattern, Payload, Proposal, Record, Request, Rule,
    Rules, Shape, StepId, Tool, TurnId, Unit, UnixMillis,
};

// ---------------------------------------------------------------------------
// The matcher against a naive reference
// ---------------------------------------------------------------------------

/// The first rule that fits, written the obvious way.
fn reference(rules: &Rules, name: Option<&ActorName>, shape: &Shape) -> Option<u64> {
    rules
        .rules
        .iter()
        .position(|rule| {
            let subject = shape.subject().map_or(&[][..], Vec::as_slice);
            rule.tool == shape.tool()
                && rule.actor.as_ref().is_none_or(|actor| Some(actor) == name)
                && match &rule.subject {
                    Pattern::Any => true,
                    Pattern::Exact(bytes) => bytes.as_slice() == subject,
                    Pattern::Prefix(bytes) => subject.starts_with(bytes),
                }
        })
        .and_then(|index| u64::try_from(index).ok())
}

fn tool() -> impl Strategy<Value = Tool> {
    prop_oneof![
        Just(Tool::Read),
        Just(Tool::Edit),
        Just(Tool::Bash),
        Just(Tool::Send),
        Just(Tool::Wake)
    ]
}

/// A call of `tool` on `subject` (a tool without a subject ignores it).
fn shape(tool: Tool, subject: Vec<u8>) -> Shape {
    match tool {
        Tool::Read => Shape::Read { subject },
        Tool::Edit => Shape::Edit { subject },
        Tool::Bash => Shape::Bash {
            subject,
            background: false,
        },
        Tool::Send => Shape::Send { subject },
        Tool::Spawn | Tool::Wake | Tool::Tasks | Tool::Cancel => Shape::Wake,
    }
}

/// Short subjects over a small alphabet, so patterns often fit.
fn bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'b')], 0..4)
}

fn name() -> impl Strategy<Value = ActorName> {
    prop_oneof![Just("main"), Just("helper")]
        .prop_map(|text| ActorName::parse(text).expect("a valid name"))
}

fn rule() -> impl Strategy<Value = Rule> {
    (
        proptest::option::of(name()),
        tool(),
        prop_oneof![
            Just(Pattern::Any),
            bytes().prop_map(Pattern::Exact),
            bytes().prop_map(Pattern::Prefix),
        ],
    )
        .prop_map(|(actor, tool, subject)| Rule {
            actor,
            tool,
            subject,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// `grant` is the first rule that fits: the reference's answer, on
    /// every input.
    #[test]
    fn grant_is_the_first_fitting_rule(
        rules in proptest::collection::vec(rule(), 0..6),
        name in proptest::option::of(name()),
        tool in tool(),
        subject in bytes(),
    ) {
        let rules = Rules { rules };
        let shape = shape(tool, subject);
        prop_assert_eq!(
            grant(&rules, name.as_ref(), &shape),
            reference(&rules, name.as_ref(), &shape)
        );
    }
}

// ---------------------------------------------------------------------------
// The core grants by rule, or asks
// ---------------------------------------------------------------------------

/// A payload whose calls carry their subject: a call's arguments are its
/// shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Shaped;

impl Payload for Shaped {
    type Text = Unit;
    type Parts = Unit;
    type Args = Shape;
    type Reason = Unit;
    type Output = Unit;

    fn shape(args: &Shape) -> Shape {
        args.clone()
    }

    fn tool(args: &Shape) -> Tool {
        args.tool()
    }
}

fn limits() -> Limits {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    Limits::new(LimitsConfig {
        turns_per_actor: count(4),
        messages_per_actor: count(4),
        tasks_per_actor: count(4),
        depth: 1,
        steps_per_turn: NonZeroU32::new(4).expect("non-zero"),
        calls_per_step: count(4),
        calls_per_turn: count(8),
        context_bytes: NonZeroU64::new(1_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    })
}

fn at(number: u64) -> UnixMillis {
    UnixMillis::new(1_700_000_000_000u64.saturating_add(number)).expect("a 48-bit time")
}

fn actor() -> ActorId {
    ActorId::from_parts(at(0), &[4; 10])
}

/// The actor named `name`, born.
fn born(name: &str) -> Record<Shaped> {
    Record::Born {
        born: Born::root(
            actor(),
            named(name),
            vec![Tool::Read, Tool::Edit, Tool::Bash],
        ),
    }
}

/// The user's message `number`.
fn envelope(number: u64) -> Envelope<Shaped> {
    Envelope {
        id: EnvelopeId::User(MessageId::from_parts(at(number), &[2; 10])),
        from: Address::User,
        to: actor(),
        reply_to: None,
        body: Body::Text(Measured {
            body: Unit,
            bytes: 1,
        }),
    }
}

fn call(name: &str, tool: Tool, subject: &str) -> Proposal<Shaped> {
    Proposal {
        id: CallId::parse(name).expect("a protocol id"),
        request: Request::Run {
            args: shape(tool, subject.as_bytes().to_vec()),
        },
    }
}

fn named(text: &str) -> ActorName {
    ActorName::parse(text).expect("a valid name")
}

/// `cargo test…` for anyone; any edit, only for `main`.
fn rules() -> Rules {
    Rules {
        rules: vec![
            Rule {
                actor: None,
                tool: Tool::Bash,
                subject: Pattern::Prefix(b"cargo test".to_vec()),
            },
            Rule {
                actor: Some(named("main")),
                tool: Tool::Edit,
                subject: Pattern::Any,
            },
        ],
    }
}

struct Driver {
    core: Core<Shaped>,
    log: Vec<Record<Shaped>>,
}

impl Driver {
    /// A core with `records` already durable.
    fn with(records: Vec<Record<Shaped>>) -> Self {
        let core = Core::recover(limits(), &records).expect("the records apply");
        Self { core, log: records }
    }

    fn send(&mut self, event: Event<Shaped>) -> (Outcome, Vec<Effect<Shaped>>) {
        let Decision {
            records,
            effects,
            outcome,
        } = self.core.decide(event);
        for record in records {
            self.core
                .apply(&record)
                .expect("the core applies its records");
            self.log.push(record);
        }
        (outcome, effects)
    }

    /// Starts a turn whose model proposes `calls`; the records of the step.
    fn propose(&mut self, number: u64, calls: Vec<Proposal<Shaped>>) -> Vec<Record<Shaped>> {
        let (_, effects) = self.send(Event::Deliver {
            envelope: envelope(number),
        });
        let id = effects
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
            .expect("a model call");
        let from = self.log.len();
        self.send(Event::ModelDone {
            id,
            result: ModelExit::Finished(calls),
        });
        self.log.get(from..).unwrap_or_default().to_vec()
    }
}

fn grants(records: &[Record<Shaped>]) -> Vec<(CallIx, Grant)> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::Approved { call, by } => Some((*call, *by)),
            Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        })
        .collect()
}

/// C1: a rule grants exactly the calls it fits, by tool, subject and the
/// actor's own name; every other call asks the user.
#[test]
fn a_rule_grants_only_what_it_fits() {
    let loaded = Record::RulesLoaded {
        rules: rules(),
        file: None,
    };
    let calls = vec![
        call("tests", Tool::Bash, "cargo test -p core"),
        call("rm", Tool::Bash, "rm -rf target"),
        call("edit", Tool::Edit, "src/lib.rs"),
        call("read", Tool::Read, "README.md"),
    ];
    let mut main = Driver::with(vec![born("main"), loaded.clone()]);
    assert_eq!(
        grants(&main.propose(1, calls.clone())),
        vec![
            (CallIx(0), Grant::Rule { rule: 0, rules: 1 }),
            (CallIx(2), Grant::Rule { rule: 1, rules: 1 }),
        ],
        "`main` may also edit; `rm` and the read ask (no rule allows reads)"
    );
    let mut helper = Driver::with(vec![born("helper"), loaded]);
    assert_eq!(
        grants(&helper.propose(1, calls)),
        vec![(CallIx(0), Grant::Rule { rule: 0, rules: 1 })],
        "a rule naming `main` grants nothing to `helper`"
    );
}

/// C1: a grant names the rules in force by their record. Once new rules
/// load, a grant naming the old ones does not apply.
#[test]
fn rules_pin_the_version_in_force() {
    let loaded = Record::RulesLoaded {
        rules: rules(),
        file: None,
    };
    let mut driver = Driver::with(vec![loaded.clone(), born("main")]);
    let step = driver.propose(1, vec![call("tests", Tool::Bash, "cargo test")]);
    assert_eq!(
        grants(&step),
        vec![(CallIx(0), Grant::Rule { rule: 0, rules: 0 })],
        "granted by the rules at seq 0"
    );
    let first = StepId {
        turn: TurnId(3),
        index: 0,
    };
    let mut log = vec![
        loaded.clone(),
        born("main"),
        Record::Delivered {
            envelope: envelope(1),
        },
        Record::TurnStarted { turn: TurnId(3) },
        Record::StepStarted {
            step: first,
            carries: vec![envelope(1).id],
        },
        Record::StepFinished {
            step: first,
            calls: vec![call("tests", Tool::Bash, "cargo test")],
        },
        loaded,
    ];
    let mut stale = log.clone();
    stale.push(Record::Approved {
        call: CallIx(0),
        by: Grant::Rule { rule: 0, rules: 0 },
    });
    assert_eq!(
        Core::recover(limits(), &stale),
        Err(Corrupt {
            seq: 7,
            violation: Violation::WrongGrant
        }),
        "the rules at seq 0 are no longer in force"
    );
    log.push(Record::Approved {
        call: CallIx(0),
        by: Grant::Rule { rule: 0, rules: 6 },
    });
    assert!(
        Core::recover(limits(), &log).is_ok(),
        "the rules at seq 6 are"
    );
}
