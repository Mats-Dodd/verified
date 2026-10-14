//! Grants (A2): the matcher against a naive reference, and the core
//! granting by rule, or asking, call by call.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use proptest::prelude::*;
use verified_core::rules::grant;
use verified_core::{Core, Corrupt, Decision, Effect, Event, ModelExit, Outcome, Violation};
use verified_domain::{
    ActorName, Born, CallId, CallIx, Grant, Limits, LimitsConfig, Measured, MessageId, Millis,
    Pattern, Payload, Proposal, Record, Request, Rule, Rules, Shape, StepId, Tool, TurnId, Unit,
    UnixMillis,
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
            rule.tool == shape.tool
                && rule.actor.as_ref().is_none_or(|actor| Some(actor) == name)
                && match &rule.subject {
                    Pattern::Any => true,
                    Pattern::Exact(bytes) => *bytes == shape.subject,
                    Pattern::Prefix(bytes) => shape.subject.starts_with(bytes),
                }
        })
        .and_then(|index| u64::try_from(index).ok())
}

fn tool() -> impl Strategy<Value = Tool> {
    prop_oneof![
        Just(Tool::Read),
        Just(Tool::Edit),
        Just(Tool::Bash),
        Just(Tool::Send)
    ]
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
        let shape = Shape { tool, subject };
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
        args.tool
    }
}

fn limits() -> Limits {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    Limits::new(LimitsConfig {
        turns_per_session: count(4),
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

fn turn(number: u64) -> TurnId {
    TurnId::from_parts(at(number), &[1; 10])
}

fn call(name: &str, tool: Tool, subject: &str) -> Proposal<Shaped> {
    Proposal {
        id: CallId::parse(name).expect("a protocol id"),
        request: Request::Run {
            args: Shape {
                tool,
                subject: subject.as_bytes().to_vec(),
            },
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
        let (_, effects) = self.send(Event::Message {
            message: MessageId::from_parts(at(number), &[2; 10]),
            turn: turn(number),
            text: Measured {
                body: Unit,
                bytes: 1,
            },
        });
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::CallModel { id, .. } => Some(*id),
                Effect::CancelModel { .. }
                | Effect::AskApproval { .. }
                | Effect::RunTool { .. }
                | Effect::CancelTool { .. }
                | Effect::Sleep { .. } => None,
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

/// A2: a rule grants exactly the calls it fits, by tool, subject and the
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
    let mut unnamed = Driver::with(vec![loaded.clone()]);
    assert_eq!(
        grants(&unnamed.propose(1, calls.clone())),
        vec![(CallIx(0), Grant::Rule { rule: 0, rules: 0 })],
        "an actor with no name: only `cargo test…` is granted"
    );
    let mut main = Driver::with(vec![
        Record::Born {
            born: Born {
                name: named("main"),
            },
        },
        loaded.clone(),
    ]);
    assert_eq!(
        grants(&main.propose(1, calls.clone())),
        vec![
            (CallIx(0), Grant::Rule { rule: 0, rules: 1 }),
            (CallIx(2), Grant::Rule { rule: 1, rules: 1 }),
        ],
        "`main` may also edit; `rm` and the read ask (no rule allows reads)"
    );
    let mut helper = Driver::with(vec![
        Record::Born {
            born: Born {
                name: named("helper"),
            },
        },
        loaded,
    ]);
    assert_eq!(
        grants(&helper.propose(1, calls)),
        vec![(CallIx(0), Grant::Rule { rule: 0, rules: 1 })],
        "a rule naming `main` grants nothing to `helper`"
    );
}

/// A2: a grant names the rules in force by their record. Once new rules
/// load, a grant naming the old ones does not apply.
#[test]
fn rules_pin_the_version_in_force() {
    let mut driver = Driver::with(vec![Record::RulesLoaded {
        rules: rules(),
        file: None,
    }]);
    let step = driver.propose(1, vec![call("tests", Tool::Bash, "cargo test")]);
    assert_eq!(
        grants(&step),
        vec![(CallIx(0), Grant::Rule { rule: 0, rules: 0 })],
        "granted by the rules at seq 0"
    );
    let reloaded = Record::RulesLoaded {
        rules: rules(),
        file: None,
    };
    let mut log = vec![
        Record::RulesLoaded {
            rules: rules(),
            file: None,
        },
        Record::TurnStarted {
            turn: turn(1),
            message: MessageId::from_parts(at(1), &[2; 10]),
            text: Measured {
                body: Unit,
                bytes: 1,
            },
        },
        Record::StepStarted {
            step: StepId {
                turn: turn(1),
                index: 0,
            },
        },
        Record::StepFinished {
            step: StepId {
                turn: turn(1),
                index: 0,
            },
            calls: vec![call("tests", Tool::Bash, "cargo test")],
        },
        reloaded,
    ];
    let mut stale = log.clone();
    stale.push(Record::Approved {
        call: CallIx(0),
        by: Grant::Rule { rule: 0, rules: 0 },
    });
    assert_eq!(
        Core::recover(limits(), &stale),
        Err(Corrupt {
            seq: 5,
            violation: Violation::WrongGrant
        }),
        "the rules at seq 0 are no longer in force"
    );
    log.push(Record::Approved {
        call: CallIx(0),
        by: Grant::Rule { rule: 0, rules: 4 },
    });
    assert!(
        Core::recover(limits(), &log).is_ok(),
        "the rules at seq 4 are"
    );
}
