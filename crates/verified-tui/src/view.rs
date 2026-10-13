//! Renders the app to the terminal: turns as styled lines (Markdown for
//! replies, a summary per tool call), the approval prompt and the status line.
//! Ported from q's `view.rs`, with tool parts added.

use ratatui::style::{Color, Modifier, Style, Stylize as _};
use ratatui::text::{Line, Span, Text};
use serde_json::Value;
use verified_wire::chunk::TurnEndReason;
use verified_wire::stream::Freshness;
use verified_wire::transcript::{Part, ToolPart, ToolState, TranscriptTurn};

const USER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const ERROR: Style = Style::new().fg(Color::Red);
const QUIET: Style = Style::new().fg(Color::DarkGray);
const TOOL: Style = Style::new().fg(Color::Magenta);
const PROMPT: Style = Style::new().fg(Color::Yellow);

/// The longest tool input or output summary shown, in characters.
const SUMMARY_CHARS: usize = 120;

/// A turn as lines: the user's message, the reply, and how it ended.
#[must_use]
pub fn turn_lines(turn: &TranscriptTurn) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, line) in turn.text().lines().enumerate() {
        let prefix = if index == 0 { "› " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, USER),
            Span::styled(line.to_owned(), USER),
        ]));
    }
    lines.push(Line::default());

    let reply = turn.reply();
    let parts = reply.map_or(&[][..], |reply| reply.parts);
    let said_anything = parts.iter().any(|part| !matches!(part, Part::StepStart));
    for part in parts {
        match part {
            Part::StepStart => {}
            Part::Text { text, .. } => lines.extend(markdown(text)),
            Part::Tool(tool) => lines.extend(tool_lines(tool)),
        }
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

/// The prompt for a tool call awaiting approval.
#[must_use]
pub fn approval_line(tool: &ToolPart) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" allow {}? ", tool.tool_name), PROMPT.bold()),
        Span::styled("y allow · n deny · esc cancel turn", QUIET),
    ])
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
    pub session: &'model str,
    pub model: Option<&'model str>,
    pub connection: &'model Connection,
    pub activity: Activity,
    pub notice: Option<&'model str>,
}

/// What the conversation is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    CreatingSession,
    Sending,
    Replying,
}

#[must_use]
pub fn status_line(status: &Status<'_>) -> Line<'static> {
    let mut spans = vec![
        " verified ".bold().reversed(),
        " ".into(),
        Span::raw(status.session.to_owned()),
        " ".into(),
    ];
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
        Activity::CreatingSession => spans.push("  ◆ starting".magenta()),
        Activity::Sending => spans.push("  ◆ sending".magenta()),
        Activity::Replying => spans.push("  ◆ replying".magenta()),
    }
    if let Some(notice) = status.notice {
        spans.push(Span::styled(format!("  {notice}"), PROMPT));
    }
    spans.push(Span::styled(
        "   ⏎ send · ⇧⏎ newline · esc cancel · ^c quit",
        QUIET,
    ));
    Line::from(spans)
}
