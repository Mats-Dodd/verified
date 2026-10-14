//! The core commutes with α: on the daemon's payload (`Parsed`), `decide`
//! and `resume` on a state decide, under α, what they decide on α of that
//! state (`recover` of α of its log). So the model, which runs the core on
//! `Abstract`, decides what the daemon's core decides, sizes and all.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use proptest::prelude::*;
use verified_core::{Core, Decision, Effect, Event, ModelExit, Verdict};
use verified_domain::{
    CallId, EffectId, Epoch, Exit, Limits, LimitsConfig, MessageId, Millis, ModelFault, Record,
    ToolName, TurnId, UnixMillis, Unstarted,
};
use verified_model::alpha;
use verified_model::alpha::record;
use verified_server::payload::{ModelPart, Parsed, ToolInput, ToolOutput, measure, proposal};

/// Small bounds, so that sizes reach the context limit and calls reach
/// theirs.
fn limits() -> Limits {
    let count = |value: usize| NonZeroUsize::new(value).expect("non-zero");
    Limits::new(LimitsConfig {
        turns_per_session: count(3),
        steps_per_turn: NonZeroU32::new(3).expect("non-zero"),
        calls_per_step: count(2),
        calls_per_turn: count(3),
        context_bytes: NonZeroU64::new(2_000).expect("non-zero"),
        retry_schedule: vec![Millis::new(10)],
        parts_per_batch: count(64),
        batch_window: Millis::new(30),
    })
}

/// A tool call as the model might send it, valid or not.
#[derive(Clone, Debug)]
enum Call {
    Read(u16),
    Edit(u16),
    Bash(u16),
    /// A tool v0 does not offer.
    Unknown,
    /// A path that escapes the project.
    Escape,
}

/// An input, resolved against whatever is outstanding. Sizes are lengths
/// of text.
#[derive(Clone, Debug)]
enum Choice {
    Message(u8, u16),
    Approve(u8, bool),
    /// Of the running turn, or of a turn that is not running.
    Interrupt(bool),
    Parts(u8, Vec<u16>),
    Finish(u8, Vec<Call>),
    Fail(u8, u8),
    ToolDone(u8, u8, u16),
    Woke(u8),
    /// A completion for an effect that never was.
    Stale(u8, u16),
    Crash,
}

fn call() -> impl Strategy<Value = Call> {
    prop_oneof![
        (0u16..400).prop_map(Call::Read),
        (0u16..400).prop_map(Call::Edit),
        (0u16..400).prop_map(Call::Bash),
        Just(Call::Unknown),
        Just(Call::Escape),
    ]
}

fn choice() -> impl Strategy<Value = Choice> {
    prop_oneof![
        (0u8..3, 0u16..600).prop_map(|(slot, size)| Choice::Message(slot, size)),
        (any::<u8>(), any::<bool>()).prop_map(|(pick, yes)| Choice::Approve(pick, yes)),
        any::<bool>().prop_map(Choice::Interrupt),
        (any::<u8>(), proptest::collection::vec(0u16..500, 1..4))
            .prop_map(|(pick, sizes)| Choice::Parts(pick, sizes)),
        (any::<u8>(), proptest::collection::vec(call(), 0..4))
            .prop_map(|(pick, calls)| Choice::Finish(pick, calls)),
        (any::<u8>(), 0u8..4).prop_map(|(pick, cause)| Choice::Fail(pick, cause)),
        (any::<u8>(), 0u8..3, 0u16..900)
            .prop_map(|(pick, exit, size)| Choice::ToolDone(pick, exit, size)),
        any::<u8>().prop_map(Choice::Woke),
        (0u8..2, any::<u16>()).prop_map(|(kind, seq)| Choice::Stale(kind, seq)),
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

fn ids(slot: u8) -> (MessageId, TurnId) {
    let at = UnixMillis::new(1_760_000_000_000 + u64::from(slot)).expect("a 48-bit time");
    (
        MessageId::from_parts(at, &[slot; 10]),
        TurnId::from_parts(at, &[slot.wrapping_add(1); 10]),
    )
}

fn text(size: u16) -> String {
    "é ".repeat(usize::from(size))
}

const fn cause(number: u8) -> ModelFault {
    match number {
        0 => ModelFault::Overloaded,
        1 => ModelFault::Network,
        2 => ModelFault::Cancelled,
        _ => ModelFault::Other,
    }
}

/// Every id ever handed out, live or stale.
#[derive(Default)]
struct Seen {
    models: Vec<EffectId>,
    tools: Vec<EffectId>,
    sleeps: Vec<EffectId>,
    asks: Vec<CallId>,
    /// Call ids minted so far, so that each is unique.
    calls: u32,
}

impl Seen {
    fn note(&mut self, effects: &[Effect<Parsed>]) {
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

    fn proposals(&mut self, calls: &[Call]) -> Vec<verified_domain::Proposal<Parsed>> {
        calls
            .iter()
            .map(|call| {
                self.calls = self.calls.saturating_add(1);
                let id = CallId::parse(&format!("toolu_{}", self.calls)).expect("a protocol id");
                let (name, input) = match call {
                    Call::Read(size) => (
                        "read",
                        format!(r#"{{"path":"a{}"}}"#, "b".repeat(usize::from(*size))),
                    ),
                    Call::Edit(size) => (
                        "edit",
                        format!(
                            r#"{{"path":"a","old":"x","new":"{}"}}"#,
                            "y".repeat(usize::from(*size))
                        ),
                    ),
                    Call::Bash(size) => (
                        "bash",
                        format!(r#"{{"command":"echo {}"}}"#, "z".repeat(usize::from(*size))),
                    ),
                    Call::Unknown => ("write", "{}".to_owned()),
                    Call::Escape => ("read", r#"{"path":"../secret"}"#.to_owned()),
                };
                proposal(
                    &id,
                    &ToolName::parse(name).expect("a tool name"),
                    &ToolInput::parse(&input).expect("a JSON object"),
                )
            })
            .collect()
    }

    fn event(&mut self, choice: &Choice, core: &Core<Parsed>) -> Option<Event<Parsed>> {
        Some(match choice {
            Choice::Message(slot, size) => {
                let (message, turn) = ids(*slot);
                Event::Message {
                    message,
                    turn,
                    text: measure(text(*size)),
                }
            }
            Choice::Approve(index, yes) => Event::Approve {
                call: pick(&self.asks, *index)?,
                verdict: if *yes {
                    Verdict::Approve
                } else {
                    Verdict::Deny
                },
            },
            Choice::Interrupt(running) => Event::Interrupt {
                turn: if *running { core.running()? } else { ids(9).1 },
            },
            Choice::Parts(index, sizes) => Event::ModelParts {
                id: pick(&self.models, *index)?,
                parts: measure(
                    sizes
                        .iter()
                        .map(|size| ModelPart::TextDelta {
                            block: 0,
                            delta: text(*size),
                        })
                        .collect(),
                ),
            },
            Choice::Finish(index, calls) => {
                let id = pick(&self.models, *index)?;
                Event::ModelDone {
                    id,
                    result: ModelExit::Finished(self.proposals(calls)),
                }
            }
            Choice::Fail(index, number) => Event::ModelDone {
                id: pick(&self.models, *index)?,
                result: ModelExit::Failed(cause(*number)),
            },
            Choice::ToolDone(index, exit, size) => Event::ToolDone {
                id: pick(&self.tools, *index)?,
                exit: match exit {
                    0 => Exit::Ok,
                    1 => Exit::NotStarted(Unstarted::Cancelled),
                    2 => Exit::Unknown,
                    _ => Exit::Failed,
                },
                output: measure(ToolOutput::complete(text(*size))),
            },
            Choice::Woke(index) => Event::Woke {
                id: pick(&self.sleeps, *index)?,
            },
            Choice::Stale(kind, seq) => {
                let id = EffectId {
                    epoch: Epoch(7),
                    seq: u64::from(*seq),
                };
                if *kind == 0 {
                    Event::ModelDone {
                        id,
                        result: ModelExit::Finished(Vec::new()),
                    }
                } else {
                    Event::ToolDone {
                        id,
                        exit: Exit::Ok,
                        output: measure(ToolOutput::complete(String::new())),
                    }
                }
            }
            Choice::Crash => return None,
        })
    }
}

/// α of the state whose log is `log`: the core recovered from α of it.
fn abstracted(log: &[Record<Parsed>]) -> Core<verified_domain::Abstract> {
    let log: Vec<_> = log.iter().map(record).collect();
    Core::recover(limits(), &log).expect("α of a log that recovers recovers")
}

/// Persists `decision`: appends its records and applies them.
fn persist(core: &mut Core<Parsed>, log: &mut Vec<Record<Parsed>>, decision: Decision<Parsed>) {
    for record in decision.records {
        core.apply(&record).expect("decided records apply");
        log.push(record);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// `decide` and `resume` commute with α at every state a run reaches,
    /// crashes included.
    #[test]
    fn the_core_commutes_with_alpha(choices in proptest::collection::vec(choice(), 0..60)) {
        let mut core: Core<Parsed> = Core::new(limits());
        let mut log: Vec<Record<Parsed>> = Vec::new();
        let mut seen = Seen::default();
        let boot = core.resume();
        prop_assert_eq!(alpha::decision(&boot), alpha::decision(&abstracted(&log).resume()));
        seen.note(&boot.effects);
        persist(&mut core, &mut log, boot);
        for choice in &choices {
            let decision = if matches!(choice, Choice::Crash) {
                core = Core::recover(limits(), &log).expect("the log recovers");
                let resumed = core.resume();
                prop_assert_eq!(
                    alpha::decision(&resumed),
                    alpha::decision(&abstracted(&log).resume()),
                    "resume commutes with α"
                );
                resumed
            } else if let Some(event) = seen.event(choice, &core) {
                let lifted = abstracted(&log).decide(alpha::event(&event));
                let decision = core.decide(event);
                prop_assert_eq!(
                    alpha::decision(&decision),
                    alpha::decision(&lifted),
                    "decide commutes with α"
                );
                decision
            } else {
                continue;
            };
            seen.note(&decision.effects);
            persist(&mut core, &mut log, decision);
        }
    }
}
