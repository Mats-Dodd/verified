//! Projects committed records into AI SDK UI chunks: what clients see.
//!
//! A port of q's `StreamProjection.ts`, extended with tool chunks, approvals
//! and `reset-step`. Deterministic: chunks depend only on the records (and
//! the model name), so the same log always projects to the same stream.
//! Stream offsets are record seqs; a record may project to no chunks.
//!
//! Beside the AI SDK's chunks, an actor's stream says what landed in its
//! mailbox (`data-envelope`), which envelopes each step carried
//! (`data-step-carries`), and when its tasks started and ended. The same
//! fold answers who waits for the user and which tasks run
//! ([`summary`]).

use std::collections::BTreeMap;

use serde_json::json;
use verified_core::Core;
use verified_core::rules::grant;
use verified_domain::id::{ActorId, ApprovalId, CallId, PartId, TaskId, ToolName, TurnId};
use verified_domain::{
    ActorName, Body, Exit, Failure, Grant, Limit, ModelFault, Payload as _, Record, Request, Rules,
    Shape, Tool, TurnOutcome, Unstarted,
};
use verified_wire::chunk::{
    CompactedData, EffectEnd, EnvelopeData, FinishReason, StepCarriesData, TaskEndedData,
    TaskStartedData, TurnEndReason, TurnEndedData, TurnStartedData, UiChunk,
};
use verified_wire::command::{ActorState, ActorSummary, TaskSummary};

use crate::history::synthesized_text;
use crate::payload::{ModelPart, Parsed};

/// Where the current step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Closed,
    Open,
    /// The attempt was discarded; the retry continues the same UI step.
    Reset,
}

#[derive(Debug, Clone)]
struct Call {
    id: CallId,
    reason: Option<String>,
    /// What it asks for; `None` for an invalid request.
    shape: Option<Shape>,
}

#[derive(Debug, Clone)]
struct Turn {
    step: StepState,
    step_index: u32,
    calls: Vec<Call>,
    cancelling: bool,
    finish: FinishReason,
}

/// The projection's state: what earlier records said that later ones need.
#[derive(Debug, Clone)]
pub struct Projector {
    model: String,
    turn: Option<Turn>,
    /// The actor's name and the rules in force: a call no rule grants asks
    /// the user.
    name: Option<ActorName>,
    rules: Option<Rules>,
    /// The seq of the next record: a task's id is the seq of the record
    /// that started it. Every record is projected once, in order.
    seq: u64,
    /// Calls of the running turn awaiting the user's verdict, in order.
    waiting: Vec<CallId>,
    /// Every task, by id.
    tasks: BTreeMap<TaskId, TaskSummary>,
}

fn part_id(step: u32, block: u32) -> PartId {
    PartId::parse(&format!("t{step}-{block}")).expect("digits, `t` and `-` make a part id")
}

fn approval_id(call: &CallId) -> ApprovalId {
    ApprovalId::from(call)
}

fn name(text: &str) -> ToolName {
    ToolName::parse(text).expect("tool names here are lowercase words")
}

/// The name the model and the user know a tool by.
fn tool_name(tool: Tool) -> ToolName {
    name(match tool {
        Tool::Read => "read",
        Tool::Edit => "edit",
        Tool::Bash => "bash",
        Tool::Spawn => "spawn",
        Tool::Send => "send",
        Tool::Wake => "wake",
        Tool::Tasks => "tasks",
        Tool::Cancel => "cancel",
    })
}

/// An outcome without its reason.
const fn effect_end(exit: Exit) -> EffectEnd {
    match exit {
        Exit::Ok => EffectEnd::Ok,
        Exit::Failed => EffectEnd::Failed,
        Exit::NotStarted(_) => EffectEnd::NotStarted,
        Exit::Unknown => EffectEnd::Unknown,
    }
}

fn failure_text(failure: Failure) -> String {
    match failure {
        Failure::Limit(Limit::Steps) => "The turn used every model step it may take.".to_owned(),
        Failure::Model(ModelFault::TooLarge) => {
            "The model's response is too large to record.".to_owned()
        }
        Failure::Limit(Limit::Context) => {
            "The conversation is larger than the context limit.".to_owned()
        }
        Failure::Model(ModelFault::Overloaded) => {
            "The model is overloaded; retries ran out.".to_owned()
        }
        Failure::Model(ModelFault::Network) => "The network failed; retries ran out.".to_owned(),
        Failure::Model(ModelFault::Other) => "The model call failed.".to_owned(),
        Failure::Model(ModelFault::Cancelled) => "The model call was interrupted.".to_owned(),
    }
}

impl Projector {
    /// A projector for a daemon talking to `model`.
    #[must_use]
    pub const fn new(model: String) -> Self {
        Self {
            model,
            turn: None,
            name: None,
            rules: None,
            seq: 0,
            waiting: Vec::new(),
            tasks: BTreeMap::new(),
        }
    }

    /// Calls of the running turn awaiting the user's verdict, in order.
    #[must_use]
    pub fn waiting(&self) -> &[CallId] {
        &self.waiting
    }

    /// Every task so far, in the order they started.
    pub fn tasks(&self) -> impl Iterator<Item = &TaskSummary> {
        self.tasks.values()
    }

    /// The chunks one record becomes, in order.
    ///
    /// # Panics
    /// Past `u64::MAX` records, which no log holds.
    #[must_use]
    pub fn project(&mut self, record: &Record<Parsed>) -> Vec<UiChunk> {
        let seq = self.seq;
        self.seq = seq
            .checked_add(1)
            .expect("a log holds fewer than u64::MAX records");
        let mut chunks = Vec::new();
        match record {
            Record::Delivered { envelope } => chunks.push(UiChunk::Envelope {
                payload: EnvelopeData {
                    envelope: envelope.id,
                    from: envelope.from,
                    reply_to: envelope.reply_to,
                    text: match &envelope.body {
                        Body::Text(text) => Some(text.body.clone()),
                        Body::Stop => None,
                    },
                },
            }),
            Record::TaskEnded { task, exit, output } => {
                let text = output
                    .as_ref()
                    .map(|output| output.body.text.clone())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| synthesized_text(*exit, None));
                if let Some(summary) = self.tasks.get_mut(task) {
                    summary.end = Some(effect_end(*exit));
                }
                chunks.push(UiChunk::TaskEnded {
                    id: *task,
                    payload: TaskEndedData {
                        end: effect_end(*exit),
                        text,
                    },
                });
            }
            Record::Dispatched { call } => {
                let shape = self
                    .turn
                    .as_ref()
                    .and_then(|turn| turn.calls.get(call.0))
                    .and_then(|call| call.shape.as_ref())
                    .filter(|shape| shape.background());
                if let Some(shape) = shape {
                    let subject = shape
                        .subject()
                        .map(|subject| String::from_utf8_lossy(subject).into_owned());
                    self.task(TaskId(seq), tool_name(shape.tool()), subject, &mut chunks);
                }
            }
            Record::StopSent { child } => {
                self.task(
                    TaskId(seq),
                    name("stop"),
                    Some(child.to_string()),
                    &mut chunks,
                );
            }
            Record::TurnStarted { turn } => {
                let id = *turn;
                self.turn = Some(Turn {
                    step: StepState::Closed,
                    step_index: 0,
                    calls: Vec::new(),
                    cancelling: false,
                    finish: FinishReason::Stop,
                });
                self.waiting.clear();
                chunks.push(UiChunk::TurnStarted {
                    id,
                    payload: TurnStartedData {
                        model: self.model.clone(),
                    },
                });
                chunks.push(UiChunk::Start {
                    message_id: Some(id.to_string()),
                });
            }
            Record::StepStarted { step, carries } => {
                if !carries.is_empty() {
                    chunks.push(UiChunk::StepCarries {
                        id: step.turn,
                        payload: StepCarriesData {
                            envelopes: carries.clone(),
                        },
                    });
                }
                if let Some(turn) = &mut self.turn {
                    turn.step_index = step.index;
                    match turn.step {
                        StepState::Reset => {}
                        StepState::Open => {
                            chunks.push(UiChunk::FinishStep);
                            chunks.push(UiChunk::StartStep);
                        }
                        StepState::Closed => chunks.push(UiChunk::StartStep),
                    }
                    turn.step = StepState::Open;
                }
            }
            Record::ModelOutput { step, parts } => {
                if let Some(turn) = &mut self.turn {
                    for part in &parts.body {
                        part_chunks(step.index, part, turn, &mut chunks);
                    }
                }
            }
            Record::StepReset { .. } => {
                if let Some(turn) = &mut self.turn {
                    turn.step = StepState::Reset;
                    chunks.push(UiChunk::ResetStep);
                }
            }
            Record::StepFinished { calls, .. } => {
                if let Some(turn) = &mut self.turn {
                    for proposal in calls {
                        let id = proposal.id.clone();
                        let (reason, shape) = match &proposal.request {
                            Request::Run { args } => (None, Some(Parsed::shape(args))),
                            Request::Invalid { reason } => (Some(reason.clone()), None),
                        };
                        let asks = shape.as_ref().is_some_and(|shape| {
                            self.rules.as_ref().is_none_or(|rules| {
                                grant(rules, self.name.as_ref(), shape).is_none()
                            })
                        });
                        if asks && !turn.cancelling {
                            chunks.push(UiChunk::ToolApprovalRequest {
                                approval_id: approval_id(&id),
                                tool_call_id: id.clone(),
                                reason: None,
                                is_automatic: None,
                            });
                            self.waiting.push(id.clone());
                        }
                        turn.calls.push(Call { id, reason, shape });
                    }
                }
            }
            Record::Approved { call, by } => {
                if let Some(slot) = self.turn.as_ref().and_then(|turn| turn.calls.get(call.0)) {
                    let id = slot.id.clone();
                    self.waiting.retain(|waiting| *waiting != id);
                    if let Grant::Rule { rule, .. } = by {
                        chunks.push(UiChunk::ToolApprovalRequest {
                            approval_id: approval_id(&id),
                            tool_call_id: id.clone(),
                            reason: Some(format!("allowed by rule {}", rule.saturating_add(1))),
                            is_automatic: Some(true),
                        });
                    }
                    chunks.push(UiChunk::ToolApprovalResponse {
                        approval_id: approval_id(&id),
                        approved: true,
                    });
                }
            }
            Record::ToolResult { call, exit, output } => {
                if let Some(slot) = self.turn.as_ref().and_then(|turn| turn.calls.get(call.0)) {
                    self.waiting.retain(|waiting| *waiting != slot.id);
                    let text = output.as_ref().map(|output| output.body.text.clone());
                    result_chunks(slot, *exit, text, &mut chunks);
                }
            }
            Record::CancelRequested { .. } => {
                if let Some(turn) = &mut self.turn {
                    turn.cancelling = true;
                }
            }
            Record::TurnEnded { turn, outcome } => {
                let id = *turn;
                self.waiting.clear();
                if let Some(state) = self.turn.take()
                    && matches!(state.step, StepState::Open)
                {
                    chunks.push(UiChunk::FinishStep);
                    let reason = match outcome {
                        TurnOutcome::Completed => state.finish,
                        TurnOutcome::Cancelled | TurnOutcome::Failed(_) => FinishReason::Stop,
                    };
                    end_chunks(id, *outcome, reason, &mut chunks);
                } else {
                    end_chunks(id, *outcome, FinishReason::Stop, &mut chunks);
                }
            }
            Record::Born { born } => self.name = Some(born.name.clone()),
            Record::RulesLoaded { rules, .. } => self.rules = Some(rules.clone()),
            // A cut shows once its summary is written: a failed one
            // changes nothing the reader sees.
            Record::Compacted { summary, .. } => chunks.push(UiChunk::Compacted {
                payload: CompactedData {
                    summary: summary.body.clone(),
                    bytes: summary.bytes,
                },
            }),
            Record::EpochBumped { .. } | Record::CompactionStarted { .. } => {}
        }
        chunks
    }

    fn task(
        &mut self,
        id: TaskId,
        tool: ToolName,
        subject: Option<String>,
        chunks: &mut Vec<UiChunk>,
    ) {
        self.tasks.insert(
            id,
            TaskSummary {
                task_id: id,
                tool: tool.clone(),
                subject: subject.clone(),
                end: None,
            },
        );
        chunks.push(UiChunk::TaskStarted {
            id,
            payload: TaskStartedData { tool, subject },
        });
    }
}

/// Where actor `actor` is: `None` while it is unborn.
///
/// Its state and mailbox are its core's; who waits for the user and which
/// tasks run, its projection's. Both are the session's live ones, so a
/// listing costs nothing per record.
#[must_use]
pub fn summary(actor: ActorId, core: &Core<Parsed>, projector: &Projector) -> Option<ActorSummary> {
    let born = core.born()?;
    // A stopped actor's last turn runs until its runs report: it is
    // running until then.
    let state = match (core.stopped(), core.running()) {
        (_, Some(turn_id)) => ActorState::Running { turn_id },
        (true, None) => ActorState::Stopped,
        (false, None) => ActorState::Idle,
    };
    let running = projector.tasks().filter(|task| task.end.is_none()).count();
    Some(ActorSummary {
        actor_id: actor,
        name: String::from_utf8_lossy(born.name.bytes()).into_owned(),
        parent: born.parent,
        depth: born.depth,
        state,
        approvals: projector.waiting().to_vec(),
        tasks: u32::try_from(running).unwrap_or(u32::MAX),
        unread: u32::try_from(core.unread()).unwrap_or(u32::MAX),
    })
}

fn end_chunks(id: TurnId, outcome: TurnOutcome, finish: FinishReason, chunks: &mut Vec<UiChunk>) {
    let reason = match outcome {
        TurnOutcome::Completed => {
            chunks.push(UiChunk::Finish {
                finish_reason: Some(finish),
            });
            TurnEndReason::Completed
        }
        TurnOutcome::Cancelled => {
            chunks.push(UiChunk::Abort {
                reason: Some("cancelled".to_owned()),
            });
            TurnEndReason::Cancelled
        }
        TurnOutcome::Failed(failure) => {
            chunks.push(UiChunk::Error {
                error_text: failure_text(failure),
            });
            TurnEndReason::Failed
        }
    };
    chunks.push(UiChunk::TurnEnded {
        id,
        payload: TurnEndedData { reason },
    });
}

fn part_chunks(step: u32, part: &ModelPart, turn: &mut Turn, chunks: &mut Vec<UiChunk>) {
    match part {
        ModelPart::TextStart { block } => chunks.push(UiChunk::TextStart {
            id: part_id(step, *block),
        }),
        ModelPart::TextDelta { block, delta } => chunks.push(UiChunk::TextDelta {
            id: part_id(step, *block),
            delta: delta.clone(),
        }),
        ModelPart::TextEnd { block } => chunks.push(UiChunk::TextEnd {
            id: part_id(step, *block),
        }),
        ModelPart::ToolInputStart { id, tool_name } => chunks.push(UiChunk::ToolInputStart {
            tool_call_id: id.clone(),
            tool_name: tool_name.clone(),
        }),
        ModelPart::ToolInputDelta { id, delta } => chunks.push(UiChunk::ToolInputDelta {
            tool_call_id: id.clone(),
            input_text_delta: delta.clone(),
        }),
        ModelPart::ToolInputEnd { .. } => {}
        ModelPart::ToolCall {
            tool_call_id,
            tool_name,
            input,
        } => chunks.push(UiChunk::ToolInputAvailable {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            input: input.to_value(),
        }),
        ModelPart::Finish { finish_reason } => turn.finish = *finish_reason,
    }
}

fn result_chunks(call: &Call, exit: Exit, text: Option<String>, chunks: &mut Vec<UiChunk>) {
    let id = call.id.clone();
    match exit {
        Exit::Ok => chunks.push(UiChunk::ToolOutputAvailable {
            tool_call_id: id,
            output: json!({"text": text.unwrap_or_default()}),
        }),
        Exit::NotStarted(Unstarted::Denied | Unstarted::Steered) => {
            chunks.push(UiChunk::ToolApprovalResponse {
                approval_id: approval_id(&id),
                approved: false,
            });
            chunks.push(UiChunk::ToolOutputDenied { tool_call_id: id });
        }
        Exit::Failed
        | Exit::NotStarted(
            Unstarted::Cancelled
            | Unstarted::Stopped
            | Unstarted::Limit
            | Unstarted::Invalid
            | Unstarted::Refused,
        )
        | Exit::Unknown => {
            let error_text = text
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| synthesized_text(exit, call.reason.as_deref()));
            chunks.push(UiChunk::ToolOutputError {
                tool_call_id: id,
                error_text,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use verified_domain::id::{ActorId, CallIx, MessageId, StepId, TaskId, ToolName, TurnId};
    use verified_domain::{
        ActorName, Address, Body, Born, Envelope, EnvelopeId, Exit, Grant, Proposal, Record,
        Request, Rules, Tool,
    };
    use verified_wire::chunk::{EffectEnd, UiChunk};

    use super::Projector;
    use crate::payload::{Boundary, Parsed, ToolArgs, ToolInput, measure};
    use crate::seam::{OsEntropy, SystemClock};

    fn actor(byte: u8) -> ActorId {
        ActorId::parse(&format!("01a10400-0000-7000-8000-0000000000{byte:02x}")).expect("an id")
    }

    /// Who the actor is and the default rules (`read: allow`).
    fn boot() -> Vec<Record<Parsed>> {
        vec![
            Record::Born {
                born: Born::root(
                    actor(1),
                    ActorName::parse("main").expect("a name"),
                    vec![Tool::Read, Tool::Edit],
                ),
            },
            Record::RulesLoaded {
                rules: Rules::default(),
                file: None,
            },
        ]
    }

    fn call(id: &str, tool: &str, input: &str) -> Proposal<Parsed> {
        let args = ToolArgs::parse(
            &ToolName::parse(tool).expect("a tool"),
            &ToolInput::parse(input).expect("an object"),
            &Boundary {
                tools: &[Tool::Read, Tool::Edit, Tool::Bash],
                text_bytes: usize::MAX,
                clock: &SystemClock,
                entropy: &OsEntropy,
            },
        )
        .expect("valid arguments");
        Proposal {
            id: verified_domain::id::CallId::parse(id).expect("a call id"),
            request: Request::Run { args },
        }
    }

    fn project(records: &[Record<Parsed>]) -> (Projector, Vec<Vec<UiChunk>>) {
        let mut projector = Projector::new("m".to_owned());
        let chunks = records
            .iter()
            .map(|record| projector.project(record))
            .collect();
        (projector, chunks)
    }

    /// What lands is announced at once; the step that carries it says so
    /// before it starts; a task starts at the record that starts it (its
    /// id is that record's seq) and ends once, with what v knows.
    #[test]
    fn mail_lands_then_a_step_carries_it_and_tasks_start_and_end() {
        let message = MessageId::parse("01a10400-0000-7000-8000-0000000000a1").expect("an id");
        let turn = TurnId(3);
        let mut records = boot();
        records.extend([
            Record::Delivered {
                envelope: Envelope {
                    id: EnvelopeId::User(message),
                    from: Address::User,
                    to: actor(1),
                    reply_to: None,
                    body: Body::Text(measure("hi".to_owned())),
                },
            },
            Record::TurnStarted { turn },
            Record::StepStarted {
                step: StepId { turn, index: 0 },
                carries: vec![EnvelopeId::User(message)],
            },
            Record::StopSent { child: actor(2) },
            Record::TaskEnded {
                task: TaskId(5),
                exit: Exit::Unknown,
                output: None,
            },
        ]);
        let (projector, chunks) = project(&records);
        let kinds: Vec<Vec<&str>> = chunks
            .iter()
            .map(|chunks| chunks.iter().map(UiChunk::kind).collect())
            .collect();
        assert_eq!(
            kinds,
            [
                vec![],
                vec![],
                vec!["data-envelope"],
                vec!["data-turn-start", "start"],
                vec!["data-step-carries", "start-step"],
                vec!["data-task-start"],
                vec!["data-task-end"],
            ]
        );
        assert!(matches!(
            chunks.get(5).and_then(|chunks| chunks.first()),
            Some(UiChunk::TaskStarted { id: TaskId(5), payload })
                if payload.tool.as_str() == "stop" && payload.subject == Some(actor(2).to_string())
        ));
        let tasks: Vec<_> = projector.tasks().collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks.first().and_then(|task| task.end),
            Some(EffectEnd::Unknown),
            "ended once, as v knows it"
        );
    }

    /// The calls no rule grants wait for the user until a grant or a result
    /// answers them; the turn's end clears what is left.
    #[test]
    fn calls_wait_for_the_user_until_answered() {
        let turn = TurnId(2);
        let mut records = boot();
        records.extend([
            Record::TurnStarted { turn },
            Record::StepStarted {
                step: StepId { turn, index: 0 },
                carries: Vec::new(),
            },
            Record::StepFinished {
                step: StepId { turn, index: 0 },
                calls: vec![
                    call("toolu_1", "read", r#"{"path":"a"}"#),
                    call("toolu_2", "edit", r#"{"path":"a","old":"x","new":"y"}"#),
                    call("toolu_3", "edit", r#"{"path":"b","old":"x","new":"y"}"#),
                ],
            },
        ]);
        let (mut projector, _) = project(&records);
        let ids = |projector: &Projector| -> Vec<String> {
            projector
                .waiting()
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert_eq!(
            ids(&projector),
            ["toolu_2", "toolu_3"],
            "the read is allowed"
        );
        drop(projector.project(&Record::Approved {
            call: CallIx(1),
            by: Grant::User,
        }));
        assert_eq!(ids(&projector), ["toolu_3"]);
        drop(projector.project(&Record::TurnEnded {
            turn,
            outcome: verified_domain::TurnOutcome::Cancelled,
        }));
        assert_eq!(ids(&projector), Vec::<String>::new());
    }
}
