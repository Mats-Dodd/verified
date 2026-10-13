//! Application model and `update`.
//!
//! One session's conversation, live, and a box to talk in. The stream is the
//! truth: what the conversation looks like is the session's stream folded
//! into a [`Transcript`]; the app itself keeps only UI state (the draft, the
//! message in flight, scroll, notices). Every id it sends was minted here from
//! a [`Stamp`] and is reused when the same command is retried.
//!
//! Ported from q's `app.rs`, trimmed to v0: no session picker, one session
//! per run, plus approvals.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position as CursorPosition};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};
use verified_domain::id::{CallId, MessageId, SessionId, TurnId};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{
    Approve, CreateSession, MessageText, Rejection, SendMessage, Verdict,
};
use verified_wire::stream::{Freshness, Offset};
use verified_wire::transcript::{Part, ToolPart, ToolState, Transcript};

use crate::Limits;
use crate::client::{Answer, ClientError, Command, Position, StreamUpdate};
use crate::tea::{Cmd, Feedback, FollowId, Model, Stamp};
use crate::view::{self, Activity, Connection, Status};

/// Which session the program shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Create this session (minted by the caller from the seams), then follow it.
    New(SessionId),
    /// Follow this existing session.
    Open(SessionId),
}

/// What the program starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    pub start: Start,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    PageUp,
    PageDown,
}

#[derive(Debug)]
pub enum Msg {
    Type(char),
    Newline,
    Backspace,
    Paste(String),
    /// Send the draft, or resend the message whose delivery failed.
    Submit(Stamp),
    CancelTurn,
    /// Answer the approval prompt.
    Decide(Verdict),
    Scroll(Scroll),
    /// Retry what stopped: creating the session, or following the stream.
    Reconnect,
    Quit,
    Feedback(Feedback),
}

impl From<Feedback> for Msg {
    fn from(feedback: Feedback) -> Self {
        Self::Feedback(feedback)
    }
}

/// Whether the daemon has the session yet, and if not, whether creating it
/// is on its way or waiting for a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Creating(Delivery),
    Ready,
}

/// Whether a command in flight is on its way or waiting for a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sending,
    Failed,
}

/// A message the daemon has not acknowledged. Its id is reused on retry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    message_id: MessageId,
    text: MessageText,
    delivery: Delivery,
}

/// Each turn's rendered lines, keyed by revision.
#[derive(Debug)]
struct Rendered {
    turn: TurnId,
    revision: u64,
    lines: Vec<Line<'static>>,
}

/// The transcript's size at the last render, for paging.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Viewport {
    lines: u16,
    height: u16,
}

/// The last stream read the app folded.
///
/// The records `from..next` arrived as exactly these chunks. Kept (moved,
/// not copied) so an observer can compare what arrived for each offset with
/// what the daemon logged.
#[derive(Debug, Clone, PartialEq)]
pub struct Folded {
    /// How many reads the app has folded, this one included.
    pub count: u64,
    pub from: Offset,
    pub next: Offset,
    pub chunks: Vec<UiChunk>,
}

#[derive(Debug)]
pub struct App {
    limits: Limits,
    session: SessionId,
    phase: Phase,
    transcript: Transcript,
    /// Where the next read starts: just past the last folded read.
    position: Position,
    folded: Option<Folded>,
    follow: FollowId,
    connection: Connection,
    draft: String,
    pending: Option<Pending>,
    /// The tool call whose approval is being sent.
    approving: Option<CallId>,
    /// The turn whose cancellation is being sent.
    cancelling: Option<TurnId>,
    /// The last thing worth telling the user.
    notice: Option<String>,
    /// The first line shown, or `None` to follow the end.
    scroll: Option<u16>,
    rendered: Vec<Rendered>,
    viewport: Viewport,
}

const START: Position = Position {
    offset: Offset::BEGINNING,
    cursor: None,
};

impl App {
    /// The folded stream.
    #[must_use]
    pub const fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Where the next read starts.
    #[must_use]
    pub const fn position(&self) -> Position {
        self.position
    }

    /// The last read folded into the transcript, if any.
    #[must_use]
    pub const fn last_read(&self) -> Option<&Folded> {
        self.folded.as_ref()
    }

    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Following live and the last read reached the end of the stream: the
    /// transcript is the fold of everything the daemon had committed then.
    #[must_use]
    pub const fn caught_up(&self) -> bool {
        matches!(self.connection, Connection::Live(Freshness::UpToDate))
    }

    /// No message waits for the daemon's acknowledgement.
    #[must_use]
    pub const fn settled(&self) -> bool {
        self.pending.is_none() && self.approving.is_none() && self.cancelling.is_none()
    }

    /// The daemon has not acknowledged the session yet.
    #[must_use]
    pub const fn creating(&self) -> bool {
        matches!(self.phase, Phase::Creating(_))
    }

    /// The message in flight did not reach the daemon; Enter resends it with
    /// the same id.
    #[must_use]
    pub fn delivery_failed(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.delivery == Delivery::Failed)
    }

    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// The tool call in the running turn awaiting the user's approval.
    #[must_use]
    pub fn awaiting_approval(&self) -> Option<&ToolPart> {
        let reply = self.transcript.running()?.reply()?;
        reply.parts.iter().find_map(|part| match part {
            Part::Tool(tool) if tool.state == ToolState::ApprovalRequested => Some(&**tool),
            Part::Tool(_)
            | Part::StepStart
            | Part::Text { .. }
            | Part::Reasoning { .. }
            | Part::File { .. }
            | Part::ReasoningFile { .. }
            | Part::SourceUrl { .. }
            | Part::SourceDocument { .. }
            | Part::Custom { .. } => None,
        })
    }

    fn activity(&self) -> Activity {
        if self.creating() {
            Activity::CreatingSession
        } else if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.delivery == Delivery::Sending)
        {
            Activity::Sending
        } else if self.transcript.running().is_some() {
            Activity::Replying
        } else {
            Activity::Idle
        }
    }

    const fn create(&mut self) -> Cmd {
        self.phase = Phase::Creating(Delivery::Sending);
        Cmd::Send(Command::CreateSession(CreateSession {
            session_id: self.session,
        }))
    }

    /// Follows the stream from the current position with a new follower.
    fn refollow(&mut self) -> Cmd {
        self.follow = self.follow.next();
        self.connection = Connection::Connecting;
        Cmd::Follow {
            follow: self.follow,
            session: self.session,
            from: self.position,
        }
    }

    fn push_text(&mut self, text: &str) {
        let limit = self.limits.wire.max_message_text_bytes;
        if self.draft.len().saturating_add(text.len()) > limit {
            self.notice = Some(format!("a message is at most {limit} bytes"));
            return;
        }
        self.draft.push_str(text);
    }

    fn send(&mut self, pending: Pending) -> Cmd {
        let command = Command::SendMessage {
            session: self.session,
            body: SendMessage {
                message_id: pending.message_id,
                text: pending.text.clone(),
            },
        };
        self.pending = Some(Pending {
            delivery: Delivery::Sending,
            ..pending
        });
        self.scroll = None;
        Cmd::Send(command)
    }

    fn submit(&mut self, stamp: Stamp) -> Cmd {
        let draft = self.draft.trim();
        if let Some(pending) = &self.pending {
            if pending.delivery == Delivery::Failed && draft.is_empty() {
                self.notice = None;
                return self.send(pending.clone());
            }
            self.notice = Some("a message is still on its way".to_owned());
            return Cmd::None;
        }
        if draft.is_empty() {
            return Cmd::None;
        }
        if self.creating() {
            self.notice = Some("the session is still being created".to_owned());
            return Cmd::None;
        }
        if self.transcript.running().is_some() {
            self.notice = Some("a turn is running (esc cancels it)".to_owned());
            return Cmd::None;
        }
        let text = match MessageText::parse(draft.to_owned(), &self.limits.wire) {
            Ok(text) => text,
            Err(error) => {
                self.notice = Some(error.to_string());
                return Cmd::None;
            }
        };
        self.draft.clear();
        self.notice = None;
        self.send(Pending {
            message_id: MessageId::from_parts(stamp.at, &stamp.entropy),
            text,
            delivery: Delivery::Sending,
        })
    }

    fn cancel_turn(&mut self) -> Cmd {
        let Some(turn) = self
            .transcript
            .running()
            .map(verified_wire::transcript::TranscriptTurn::id)
        else {
            return Cmd::None;
        };
        if self.cancelling == Some(turn) {
            return Cmd::None;
        }
        self.cancelling = Some(turn);
        Cmd::Send(Command::Cancel {
            session: self.session,
            turn,
        })
    }

    fn decide(&mut self, verdict: Verdict) -> Cmd {
        if self.approving.is_some() {
            return Cmd::None;
        }
        let Some(call_id) = self
            .awaiting_approval()
            .map(|tool| tool.tool_call_id.clone())
        else {
            return Cmd::None;
        };
        self.approving = Some(call_id.clone());
        Cmd::Send(Command::Approve {
            session: self.session,
            body: Approve { call_id, verdict },
        })
    }

    /// Restarts what has stopped; anything still on its way is left alone,
    /// so holding the key starts nothing new.
    fn reconnect(&mut self) -> Cmd {
        match (self.phase, &self.connection) {
            (Phase::Creating(Delivery::Failed), _) => {
                self.notice = None;
                self.create()
            }
            (Phase::Ready, Connection::Broken(_)) => {
                // The fold failed part-way: start over from the beginning.
                self.transcript = Transcript::new(&self.limits.wire);
                self.rendered.clear();
                self.position = START;
                self.refollow()
            }
            (Phase::Ready, Connection::Idle | Connection::Down(_)) => self.refollow(),
            (Phase::Creating(Delivery::Sending), _)
            | (
                Phase::Ready,
                Connection::Connecting | Connection::Live(_) | Connection::Retrying(_),
            ) => Cmd::None,
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        let page = self.viewport.height.saturating_sub(2).max(1);
        let top = self.scroll.unwrap_or_else(|| self.bottom());
        self.scroll = match scroll {
            Scroll::PageUp => Some(top.saturating_sub(page)),
            Scroll::PageDown => Some(top.saturating_add(page)).filter(|top| *top < self.bottom()),
        };
    }

    /// The top line when following the end.
    const fn bottom(&self) -> u16 {
        self.viewport.lines.saturating_sub(self.viewport.height)
    }

    fn answered(&mut self, answer: Answer) -> Cmd {
        match answer {
            Answer::SessionCreated { session, result } => {
                if session != self.session || self.phase != Phase::Creating(Delivery::Sending) {
                    return Cmd::None;
                }
                match result {
                    Ok(_created_or_exists) => {
                        self.phase = Phase::Ready;
                        self.refollow()
                    }
                    Err(error) => {
                        self.phase = Phase::Creating(Delivery::Failed);
                        self.notice = Some(format!(
                            "cannot create the session: {} (^r retries)",
                            explain(&error)
                        ));
                        Cmd::None
                    }
                }
            }
            Answer::MessageSent { message_id, result } => {
                let Some(pending) = self
                    .pending
                    .take_if(|pending| pending.message_id == message_id)
                else {
                    return Cmd::None;
                };
                match result {
                    Ok(_reply) => {}
                    Err(ClientError::Rejected(rejection)) => {
                        // Refused for good: the text goes back to the draft.
                        if self.draft.is_empty() {
                            self.draft = pending.text.into_string();
                        }
                        self.notice = Some(refusal(&rejection));
                    }
                    Err(error) => {
                        self.notice = Some(format!("not sent: {error} (⏎ retries)"));
                        self.pending = Some(Pending {
                            delivery: Delivery::Failed,
                            ..pending
                        });
                    }
                }
                Cmd::None
            }
            Answer::Approved { call_id, result } => {
                if self
                    .approving
                    .take_if(|approving| *approving == call_id)
                    .is_some()
                    && let Err(error) = result
                {
                    self.notice = Some(format!("approval not sent: {}", explain(&error)));
                }
                Cmd::None
            }
            Answer::Cancelled { turn_id, result } => {
                if self
                    .cancelling
                    .take_if(|cancelling| *cancelling == turn_id)
                    .is_some()
                    && let Err(error) = result
                {
                    self.notice = Some(format!("cannot cancel: {}", explain(&error)));
                }
                Cmd::None
            }
        }
    }

    fn streamed(&mut self, follow: FollowId, update: StreamUpdate) -> Cmd {
        if follow != self.follow {
            return Cmd::None;
        }
        match update {
            StreamUpdate::Connected => {
                // Live, but nothing read on this connection says how far.
                self.connection = Connection::Live(Freshness::Behind);
                Cmd::None
            }
            StreamUpdate::Read {
                from,
                chunks,
                control,
            } => {
                // The follower resumes from the offset it last delivered and
                // the app restarts it from the one it last folded; they are
                // the same offset, or a read was skipped or folded twice.
                assert!(
                    from == self.position.offset,
                    "follower {} delivered a read from {from}, but the transcript is folded up to {}",
                    follow.0,
                    self.position.offset
                );
                for chunk in &chunks {
                    if let Err(error) = self.transcript.apply(chunk) {
                        self.connection = Connection::Broken(error.to_string());
                        // Updates the stopped follower already queued are
                        // stale: they continue a fold that was abandoned.
                        self.follow = self.follow.next();
                        return Cmd::StopFollowing;
                    }
                }
                self.position = Position {
                    offset: control.stream_next_offset,
                    cursor: Some(control.stream_cursor),
                };
                let count = self
                    .folded
                    .as_ref()
                    .map_or(1, |folded| folded.count.saturating_add(1));
                self.folded = Some(Folded {
                    count,
                    from,
                    next: control.stream_next_offset,
                    chunks,
                });
                self.connection = Connection::Live(control.up_to_date);
                Cmd::None
            }
            StreamUpdate::Disconnected { error, retry } => {
                self.connection = match retry {
                    Some(delay) => Connection::Retrying(format!(
                        "{error}; retrying in {}ms",
                        delay.as_millis()
                    )),
                    None => Connection::Down(error.to_string()),
                };
                Cmd::None
            }
            StreamUpdate::Ended { error } => {
                self.connection = Connection::Down(error.to_string());
                Cmd::None
            }
        }
    }

    /// Lines for every turn, re-rendering only those that changed.
    fn transcript_lines(&mut self) -> Vec<Line<'static>> {
        let turns = self.transcript.turns();
        self.rendered.truncate(turns.len());
        for (index, turn) in turns.iter().enumerate() {
            let fresh = || Rendered {
                turn: turn.id(),
                revision: turn.revision(),
                lines: view::turn_lines(turn),
            };
            match self.rendered.get_mut(index) {
                Some(entry) if entry.turn == turn.id() && entry.revision == turn.revision() => {}
                Some(entry) => *entry = fresh(),
                None => self.rendered.push(fresh()),
            }
        }
        self.rendered
            .iter()
            .flat_map(|rendered| rendered.lines.iter().cloned())
            .collect()
    }
}

/// What to tell the user about a refusal. The match is exhaustive on
/// purpose: each new kind of refusal gets its own thought here.
fn refusal(rejection: &Rejection) -> String {
    let hint = match rejection {
        Rejection::SessionBusy { .. } => Some("esc cancels it"),
        Rejection::SessionNotFound { .. } | Rejection::SessionFull { .. } => {
            Some("restart verified for a new session")
        }
        Rejection::TooManySessions { .. } => Some("close another session's client"),
        Rejection::TooManyFollowers { .. } => Some("close another client of this session"),
        Rejection::Malformed { .. }
        | Rejection::SessionExists { .. }
        | Rejection::MessageExistsWithDifferentText { .. }
        | Rejection::NoPendingApproval { .. }
        | Rejection::ApprovalExistsWithDifferentVerdict { .. }
        | Rejection::TurnNotRunning { .. } => None,
    };
    match hint {
        Some(hint) => format!("{rejection} ({hint})"),
        None => rejection.to_string(),
    }
}

/// A failed command, for the status line.
fn explain(error: &ClientError) -> String {
    match error {
        ClientError::Rejected(rejection) => refusal(rejection),
        ClientError::Connect(_)
        | ClientError::Http(_)
        | ClientError::Status { .. }
        | ClientError::Reply(_)
        | ClientError::TooLarge { .. }
        | ClientError::Stream(_)
        | ClientError::UnpairedData
        | ClientError::ClosedEarly
        | ClientError::Offset { .. }
        | ClientError::TimedOut { .. } => error.to_string(),
    }
}

fn is_ctrl(key: &KeyEvent, letter: char) -> bool {
    key.code == KeyCode::Char(letter) && key.modifiers.contains(KeyModifiers::CONTROL)
}

impl Model for App {
    type Msg = Msg;
    type Flags = Flags;

    fn init(flags: Flags) -> (Self, Cmd) {
        let (session, phase) = match flags.start {
            Start::New(session) => (session, Phase::Creating(Delivery::Sending)),
            Start::Open(session) => (session, Phase::Ready),
        };
        let mut app = Self {
            limits: flags.limits,
            session,
            phase,
            transcript: Transcript::new(&flags.limits.wire),
            position: START,
            folded: None,
            follow: FollowId(0),
            connection: Connection::Idle,
            draft: String::new(),
            pending: None,
            approving: None,
            cancelling: None,
            notice: None,
            scroll: None,
            rendered: Vec::new(),
            viewport: Viewport::default(),
        };
        let cmd = match phase {
            Phase::Creating(_) => app.create(),
            Phase::Ready => app.refollow(),
        };
        (app, cmd)
    }

    fn on_event(&self, event: Event, stamp: Stamp) -> Option<Msg> {
        let key = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            Event::Paste(text) => return Some(Msg::Paste(text)),
            Event::Key(_)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Mouse(_)
            | Event::Resize(..) => return None,
        };
        if is_ctrl(&key, 'c') || is_ctrl(&key, 'd') {
            return Some(Msg::Quit);
        }
        if is_ctrl(&key, 'r') {
            return Some(Msg::Reconnect);
        }
        let prompt = self.awaiting_approval().is_some();
        match key.code {
            KeyCode::Esc => Some(Msg::CancelTurn),
            KeyCode::PageUp => Some(Msg::Scroll(Scroll::PageUp)),
            KeyCode::PageDown => Some(Msg::Scroll(Scroll::PageDown)),
            // The approval prompt takes the keyboard while it is shown.
            KeyCode::Char('y' | 'Y') if prompt => Some(Msg::Decide(Verdict::Grant)),
            KeyCode::Char('n' | 'N') if prompt => Some(Msg::Decide(Verdict::Deny)),
            _ if prompt => None,
            KeyCode::Enter if key.modifiers.is_empty() => Some(Msg::Submit(stamp)),
            KeyCode::Enter => Some(Msg::Newline),
            KeyCode::Backspace => Some(Msg::Backspace),
            KeyCode::Char(letter) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Msg::Type(letter))
            }
            KeyCode::Char(_)
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Delete
            | KeyCode::Insert
            | KeyCode::F(_)
            | KeyCode::Null
            | KeyCode::CapsLock
            | KeyCode::ScrollLock
            | KeyCode::NumLock
            | KeyCode::PrintScreen
            | KeyCode::Pause
            | KeyCode::Menu
            | KeyCode::KeypadBegin
            | KeyCode::Media(_)
            | KeyCode::Modifier(_) => None,
        }
    }

    fn update(&mut self, msg: Msg) -> Cmd {
        match msg {
            Msg::Type(letter) => {
                self.push_text(letter.encode_utf8(&mut [0; 4]));
                Cmd::None
            }
            Msg::Newline => {
                self.push_text("\n");
                Cmd::None
            }
            Msg::Backspace => {
                self.draft.pop();
                Cmd::None
            }
            Msg::Paste(text) => {
                self.push_text(&text);
                Cmd::None
            }
            Msg::Submit(stamp) => self.submit(stamp),
            Msg::CancelTurn => self.cancel_turn(),
            Msg::Decide(verdict) => self.decide(verdict),
            Msg::Scroll(scroll) => {
                self.scroll(scroll);
                Cmd::None
            }
            Msg::Reconnect => self.reconnect(),
            Msg::Quit => Cmd::Quit,
            Msg::Feedback(Feedback::Answered(answer)) => self.answered(answer),
            Msg::Feedback(Feedback::Stream { follow, update }) => self.streamed(follow, update),
        }
    }

    fn view(&mut self, frame: &mut Frame<'_>) {
        let draft_lines = u16::try_from(self.draft.split('\n').count()).unwrap_or(u16::MAX);
        let input_height = draft_lines
            .clamp(1, self.limits.input_lines.max(1))
            .saturating_add(2);
        let prompt = self.awaiting_approval().map(view::approval_line);
        let prompt_height = u16::from(prompt.is_some());
        let [main, prompt_area, input, footer] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(prompt_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let transcript = Paragraph::new(self.transcript_lines())
            .wrap(Wrap { trim: false })
            .block(Block::new().padding(Padding::horizontal(1)));
        self.viewport = Viewport {
            lines: u16::try_from(transcript.line_count(main.width)).unwrap_or(u16::MAX),
            height: main.height,
        };
        let top = self
            .scroll
            .unwrap_or_else(|| self.bottom())
            .min(self.bottom());
        frame.render_widget(transcript.scroll((top, 0)), main);

        if let Some(prompt) = prompt {
            frame.render_widget(Paragraph::new(prompt), prompt_area);
        }

        let border = Block::bordered().border_style(Style::new().fg(Color::DarkGray));
        let inner = border.inner(input);
        let input_widget = if self.draft.is_empty() {
            Paragraph::new(Line::styled(
                "Say something…",
                Style::new().fg(Color::DarkGray),
            ))
        } else {
            Paragraph::new(self.draft.as_str())
        };
        let shown = usize::from(inner.height.max(1));
        let lines: Vec<&str> = self.draft.split('\n').collect();
        let hidden = u16::try_from(lines.len().saturating_sub(shown)).unwrap_or(u16::MAX);
        frame.render_widget(input_widget.block(border).scroll((hidden, 0)), input);
        let last = lines.last().copied().unwrap_or_default();
        let column = u16::try_from(last.chars().count()).unwrap_or(u16::MAX);
        let row = u16::try_from(lines.len().saturating_sub(1))
            .unwrap_or(u16::MAX)
            .saturating_sub(hidden);
        frame.set_cursor_position(CursorPosition::new(
            inner
                .x
                .saturating_add(column)
                .min(inner.right().saturating_sub(1)),
            inner.y.saturating_add(row),
        ));

        let session = self.session.to_string();
        let short = session
            .get(session.len().saturating_sub(8)..)
            .unwrap_or(&session);
        let model = self
            .transcript
            .turns()
            .iter()
            .rev()
            .find_map(|turn| turn.model());
        let status = Status {
            session: short,
            model,
            connection: &self.connection,
            activity: self.activity(),
            notice: self.notice.as_deref(),
        };
        frame.render_widget(Paragraph::new(view::status_line(&status)), footer);
    }
}

#[cfg(test)]
mod tests;
