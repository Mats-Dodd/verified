//! Renders the app to the terminal.
//!
//! Turns as styled lines (what each step carried, Markdown for replies, a
//! summary per tool call), the mail still queued, the actor tree, the task
//! panel, the approval prompt and the status line. Ported from q's
//! `view.rs`, with tool parts and actors added.

use std::collections::BTreeMap;

use ratatui::style::{Color, Modifier, Style, Stylize as _};
use ratatui::text::{Line, Span, Text};
use serde_json::Value;
use verified_domain::Address;
use verified_domain::id::{ActorId, TaskId};
use verified_wire::chunk::TurnEndReason;
use verified_wire::command::{ActorState, ActorSummary};
use verified_wire::stream::Freshness;
use verified_wire::transcript::{Mail, Part, TaskView, ToolPart, ToolState, TranscriptTurn};

const USER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const ERROR: Style = Style::new().fg(Color::Red);
const QUIET: Style = Style::new().fg(Color::DarkGray);
const TOOL: Style = Style::new().fg(Color::Magenta);
const PROMPT: Style = Style::new().fg(Color::Yellow);
const AGENT: Style = Style::new().fg(Color::Blue);

/// The longest tool input or output summary shown, in characters.
const SUMMARY_CHARS: usize = 120;

/// The last 8 characters of an actor's id: enough to tell actors apart.
fn short(actor: ActorId) -> String {
    let text = actor.to_string();
    text.get(text.len().saturating_sub(8)..)
        .unwrap_or(&text)
        .to_owned()
}

/// An envelope as lines: who sent it, and what it says. A user's message
/// reads as typed; anything else names its sender.
fn mail_lines(mail: &Mail, queued: bool) -> Vec<Line<'static>> {
    let (style, from) = match mail.from {
        Address::User => (USER, None),
        Address::Runtime => (QUIET, Some("v".to_owned())),
        Address::Actor(actor) => (AGENT, Some(short(actor))),
    };
    let mut lines = Vec::new();
    for (index, line) in mail.text.lines().enumerate() {
        let prefix = match (index, &from) {
            (0, None) => "› ".to_owned(),
            (0, Some(from)) => format!("✉ {from}: "),
            (_, _) => "  ".to_owned(),
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(line.to_owned(), style),
        ]));
    }
    if queued && let Some(last) = lines.last_mut() {
        last.push_span(Span::styled("  queued", QUIET));
    }
    lines
}

/// A turn as lines: what each step carried, the reply, and how it ended.
/// A message that steered the turn shows where it entered: before the step
/// that carried it.
#[must_use]
pub fn turn_lines(turn: &TranscriptTurn) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let carried = |lines: &mut Vec<Line<'static>>, step: usize| {
        for input in turn.inputs().iter().filter(|input| input.step == step) {
            if step > 0 {
                lines.push(Line::default());
            }
            lines.extend(mail_lines(&input.mail, false));
            lines.push(Line::default());
        }
    };
    carried(&mut lines, 0);

    let reply = turn.reply();
    let parts = reply.map_or(&[][..], |reply| reply.parts);
    let said_anything = parts.iter().any(|part| !matches!(part, Part::StepStart));
    let mut steps: usize = 0;
    for part in parts {
        match part {
            Part::StepStart => {
                if steps > 0 {
                    carried(&mut lines, steps);
                }
                steps = steps.saturating_add(1);
            }
            Part::Text { text, .. } => lines.extend(markdown(text)),
            Part::Tool(tool) => lines.extend(tool_lines(tool)),
        }
    }
    // Carried by a step whose parts have not arrived yet.
    for input in turn
        .inputs()
        .iter()
        .filter(|input| input.step >= steps.max(1))
    {
        lines.push(Line::default());
        lines.extend(mail_lines(&input.mail, false));
    }
    if turn.end().is_none() && !said_anything {
        lines.push(Line::styled("…", QUIET));
    }
    for error in turn.errors() {
        lines.push(Line::styled(format!("✗ {error}"), ERROR));
    }
    match turn.end() {
        Some(TurnEndReason::Cancelled) => lines.push(Line::styled("⏹ cancelled", QUIET)),
        Some(TurnEndReason::Failed) => lines.push(Line::styled("✗ failed", ERROR)),
        Some(TurnEndReason::Completed) | None => {}
    }
    lines.push(Line::default());
    lines
}

const fn state_label(state: ToolState) -> &'static str {
    match state {
        ToolState::InputStreaming => "preparing",
        ToolState::InputAvailable => "ready",
        ToolState::ApprovalRequested => "awaiting approval",
        ToolState::ApprovalResponded => "approved",
        ToolState::OutputAvailable => "done",
        ToolState::OutputError => "error",
        ToolState::OutputDenied => "denied",
    }
}

/// A tool call: a header, its input, and its outcome.
fn tool_lines(tool: &ToolPart) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("⚙ ", TOOL),
        Span::styled(tool.tool_name.to_string(), TOOL),
        Span::styled(format!("  {}", state_label(tool.state)), QUIET),
    ])];
    let input = tool.input.as_ref().or(tool.raw_input.as_ref());
    if let Some(input) = input {
        lines.push(Line::styled(format!("  {}", summary(input)), QUIET));
    }
    match tool.state {
        ToolState::OutputAvailable => {
            if let Some(output) = &tool.output {
                lines.push(Line::styled(format!("  → {}", summary(output)), QUIET));
            }
        }
        ToolState::OutputError => {
            let error = tool.error_text.as_deref().unwrap_or("failed");
            lines.push(Line::styled(format!("  ✗ {error}"), ERROR));
        }
        ToolState::OutputDenied
        | ToolState::InputStreaming
        | ToolState::InputAvailable
        | ToolState::ApprovalRequested
        | ToolState::ApprovalResponded => {}
    }
    lines
}

/// JSON on one line, shortened.
fn summary(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            value.to_string()
        }
    };
    let line = text.replace('\n', " ");
    if line.chars().count() <= SUMMARY_CHARS {
        return line;
    }
    let short: String = line.chars().take(SUMMARY_CHARS).collect();
    format!("{}…", short.trim_end())
}

/// Markdown, rendered and owned (the renderer borrows its input).
fn markdown(text: &str) -> Vec<Line<'static>> {
    let Text { lines, .. } = tui_markdown::from_str(text);
    lines
        .into_iter()
        .map(|line| Line {
            spans: line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect(),
            ..line
        })
        .collect()
}

/// Mail that landed and no step has carried yet: it waits for the running
/// turn's next step, or starts the next turn.
#[must_use]
pub fn queued_lines(queued: &[Mail]) -> Vec<Line<'static>> {
    queued
        .iter()
        .flat_map(|mail| mail_lines(mail, true))
        .collect()
}

/// The prompt for a tool call awaiting approval, naming the actor that
/// asks.
#[must_use]
pub fn approval_line(actor: Option<&str>, tool: &ToolPart) -> Line<'static> {
    let who = actor.map_or_else(String::new, |name| format!("{name}: "));
    Line::from(vec![
        Span::styled(format!(" {who}allow {}? ", tool.tool_name), PROMPT.bold()),
        Span::styled("y allow · n deny · esc cancel turn", QUIET),
    ])
}

/// The actors in tree order, each with its depth in the tree: every root
/// (an actor whose parent is not listed) by id, each followed by its
/// children, depth first.
#[must_use]
pub fn tree(actors: &[ActorSummary]) -> Vec<(usize, &ActorSummary)> {
    let listed = |actor: &ActorId| actors.iter().any(|other| other.actor_id == *actor);
    let mut children: BTreeMap<Option<ActorId>, Vec<&ActorSummary>> = BTreeMap::new();
    for actor in actors {
        let parent = actor.parent.filter(listed);
        children.entry(parent).or_default().push(actor);
    }
    for siblings in children.values_mut() {
        siblings.sort_by_key(|actor| actor.actor_id);
    }
    let mut order = Vec::with_capacity(actors.len());
    let mut stack: Vec<(usize, &ActorSummary)> = children
        .get(&None)
        .map(|roots| roots.iter().rev().map(|root| (0, *root)).collect())
        .unwrap_or_default();
    while let Some((depth, actor)) = stack.pop() {
        order.push((depth, actor));
        if let Some(kids) = children.get(&Some(actor.actor_id)) {
            stack.extend(kids.iter().rev().map(|kid| (depth.saturating_add(1), *kid)));
        }
    }
    order
}

/// The actor tree, at most `height` lines around the actor followed.
///
/// One line per actor, indented by depth, with what it is doing. `*` marks
/// the one followed, `?n` its calls awaiting approval, `■` a stopped actor.
#[must_use]
pub fn tree_lines(actors: &[ActorSummary], followed: ActorId, height: u16) -> Vec<Line<'static>> {
    let order = tree(actors);
    let height = usize::from(height);
    let here = order
        .iter()
        .position(|(_, actor)| actor.actor_id == followed)
        .unwrap_or(0);
    let first = here
        .saturating_add(1)
        .saturating_sub(height)
        .min(order.len().saturating_sub(height));
    order
        .iter()
        .skip(first)
        .take(height)
        .map(|(depth, actor)| {
            let mark = if actor.actor_id == followed {
                "* "
            } else {
                "  "
            };
            let mut spans = vec![
                Span::styled(mark, PROMPT),
                Span::raw("  ".repeat(*depth)),
                Span::styled(
                    actor.name.clone(),
                    if actor.actor_id == followed {
                        Style::new().bold()
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(format!(" {}", short(actor.actor_id)), QUIET),
            ];
            match actor.state {
                ActorState::Idle => {}
                ActorState::Running { .. } => spans.push("  ◆ running".magenta()),
                ActorState::Stopped => spans.push(Span::styled("  ■ stopped", QUIET)),
            }
            if !actor.approvals.is_empty() {
                spans.push(Span::styled(
                    format!("  ?{}", actor.approvals.len()),
                    PROMPT.bold(),
                ));
            }
            if actor.tasks > 0 {
                spans.push(Span::styled(format!("  {} tasks", actor.tasks), QUIET));
            }
            if actor.unread > 0 {
                spans.push(Span::styled(format!("  {} unread", actor.unread), QUIET));
            }
            Line::from(spans)
        })
        .collect()
}

/// The followed actor's running tasks, at most `height` lines; the last
/// says how many more there are when they do not fit.
#[must_use]
pub fn task_lines(tasks: &BTreeMap<TaskId, TaskView>, height: u16) -> Vec<Line<'static>> {
    let running: Vec<(&TaskId, &TaskView)> = tasks
        .iter()
        .filter(|(_, task)| task.end.is_none())
        .collect();
    let height = usize::from(height);
    let shown = if running.len() > height {
        height.saturating_sub(1)
    } else {
        running.len()
    };
    let mut lines: Vec<Line<'static>> = running
        .iter()
        .take(shown)
        .map(|(id, task)| {
            let subject = task
                .subject
                .as_deref()
                .map(|subject| summary(&Value::String(subject.to_owned())))
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!(" ⧗ task {id} "), TOOL),
                Span::styled(task.tool.to_string(), TOOL),
                Span::styled(format!("  {subject}"), QUIET),
            ])
        })
        .collect();
    if running.len() > shown && height > 0 {
        lines.push(Line::styled(
            format!(
                " ⧗ {} more tasks running",
                running.len().saturating_sub(shown)
            ),
            QUIET,
        ));
    }
    lines
}

/// Where the connection to the daemon stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// Not following yet.
    Idle,
    Connecting,
    /// Following; whether the last read on this connection reached the end.
    Live(Freshness),
    /// Dropped; reconnecting after a delay, or at once on ^r.
    Retrying(String),
    /// Stopped following; ^r reconnects.
    Down(String),
    /// The stream did not fold; ^r reloads it from the beginning.
    Broken(String),
}

/// What the status line shows.
#[derive(Debug)]
pub struct Status<'model> {
    /// The followed actor's name, once the actor list names it.
    pub name: Option<&'model str>,
    /// The followed actor's short id.
    pub actor: &'model str,
    pub model: Option<&'model str>,
    pub connection: &'model Connection,
    pub activity: Activity,
    pub notice: Option<&'model str>,
}

/// What the conversation is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    CreatingActor,
    Sending,
    Replying,
}

#[must_use]
pub fn status_line(status: &Status<'_>) -> Line<'static> {
    let mut spans = vec![" verified ".bold().reversed(), " ".into()];
    if let Some(name) = status.name {
        spans.push(Span::raw(format!("{name} ")));
    }
    spans.push(Span::styled(format!("{} ", status.actor), QUIET));
    if let Some(model) = status.model {
        spans.push(Span::styled(model.to_owned(), QUIET));
        spans.push(" ".into());
    }
    spans.push(match status.connection {
        Connection::Idle => Span::styled("○", QUIET),
        Connection::Connecting => "◌ connecting".yellow(),
        Connection::Live(_) => "● live".green(),
        Connection::Retrying(reason) => Span::styled(format!("◌ {reason} (^r now)"), PROMPT),
        Connection::Down(reason) => Span::styled(format!("✗ {reason} (^r)"), ERROR),
        Connection::Broken(reason) => Span::styled(format!("✗ {reason} (^r reloads)"), ERROR),
    });
    match status.activity {
        Activity::Idle => {}
        Activity::CreatingActor => spans.push("  ◆ starting".magenta()),
        Activity::Sending => spans.push("  ◆ sending".magenta()),
        Activity::Replying => spans.push("  ◆ replying".magenta()),
    }
    if let Some(notice) = status.notice {
        spans.push(Span::styled(format!("  {notice}"), PROMPT));
    }
    spans.push(Span::styled(
        "   ⏎ send · ⇧⏎ newline · tab actor · esc cancel · ^c quit",
        QUIET,
    ));
    Line::from(spans)
}
