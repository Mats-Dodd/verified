//! Application model and `update`.
//!
//! One actor's conversation, live, and a box to talk in. The stream is the
//! truth: what the conversation looks like is the followed actor's stream
//! folded into a [`Transcript`]; the app itself keeps only UI state (the
//! draft, the message in flight, scroll, notices) and the daemon's actor
//! list, refreshed on a tick. Every id it sends was minted here from a
//! [`Stamp`] and is reused when the same command is retried.
//!
//! The actor tree on top shows every actor, who is running, who waits for
//! the user and who is stopped; Tab and Shift-Tab follow another one, and
//! approvals, Esc and Ctrl-K (stop, once `y` confirms it) go to the actor
//! followed. A message typed while a turn runs is sent at once: it steers
//! the turn, and shows as queued until a step carries it. While an
//! approval waits, `y` and `n` answer it on an empty draft; any other text
//! is typed, and Enter sends it too, denying the waiting calls.
//!
//! Ported from q's `app.rs`, plus approvals and actors.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position as CursorPosition};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};
use verified_domain::id::{ActorId, CallId, MessageId, TurnId};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{
    ActorSummary, Approve, CreateActor, MessageText, Rejection, SendMessage, StopActor, Verdict,
};
use verified_wire::stream::{Freshness, Offset};
use verified_wire::transcript::{Part, ToolPart, ToolState, Transcript};

use crate::Limits;
use crate::client::{Answer, ClientError, Command, StreamUpdate};
use crate::tea::{Cmd, Feedback, FollowId, Stamp};
use crate::view::{self, Activity, Connection, Status};

/// Which actor the program shows first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Create this root actor (minted by the caller from the seams), then
    /// follow it.
    New(ActorId),
    /// Follow this existing actor.
    Open(ActorId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Previous,
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
    /// Ask to stop the actor followed (Ctrl-K): `y` confirms it.
    AskStop(Stamp),
    /// Confirm the stop asked for (`true`), or drop it.
    ConfirmStop(bool),
    /// Answer the approval prompt.
    Decide(Verdict),
    /// Follow the next actor in the tree (Tab), or the previous (Shift-Tab).
    Switch(Direction),
    Scroll(Scroll),
    /// Retry what stopped: creating the session, or following the stream.
    Reconnect,
    Quit,
    Feedback(Feedback),
}

/// Whether the daemon has the actor yet, and if not, whether creating it
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
    actor: ActorId,
    message_id: MessageId,
    text: MessageText,
    delivery: Delivery,
}

/// A stop of an actor: asked for and awaiting the user's `y`, or on its
/// way. Its id is reused on retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stopping {
    actor: ActorId,
    stop_id: MessageId,
    confirmed: bool,
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
    /// The actor followed: its stream is the transcript.
    actor: ActorId,
    phase: Phase,
    /// The daemon's actors, as last listed.
    actors: Vec<ActorSummary>,
    /// A listing is on its way.
    listing: bool,
    transcript: Transcript,
    /// Where the next read starts: just past the last folded read.
    offset: Offset,
    folded: Option<Folded>,
    follow: FollowId,
    connection: Connection,
    draft: String,
    pending: Option<Pending>,
    /// The tool call whose approval is being sent.
    approving: Option<CallId>,
    /// The turn whose cancellation is being sent.
    cancelling: Option<TurnId>,
    /// The stop being confirmed or sent.
    stopping: Option<Stopping>,
    /// The last thing worth telling the user.
    notice: Option<String>,
    /// The first line shown, or `None` to follow the end.
    scroll: Option<u16>,
    rendered: Vec<Rendered>,
    viewport: Viewport,
}

impl App {
    /// The folded stream.
    #[must_use]
    pub const fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The actor followed.
    #[must_use]
    pub const fn actor(&self) -> ActorId {
        self.actor
    }

    /// The daemon's actors, as last listed.
    #[must_use]
    pub fn actors(&self) -> &[ActorSummary] {
        &self.actors
    }

    /// Where the next read starts.
    #[must_use]
    pub const fn offset(&self) -> Offset {
        self.offset
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
        self.pending.is_none()
            && self.approving.is_none()
            && self.cancelling.is_none()
            && self.stopping.is_none()
    }

    /// A stop awaits the user's `y`.
    #[must_use]
    pub fn confirming_stop(&self) -> bool {
        self.stopping.is_some_and(|stopping| !stopping.confirmed)
    }

    /// The daemon has not acknowledged the actor yet.
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
            Part::Tool(_) | Part::StepStart | Part::Text { .. } => None,
        })
    }

    fn activity(&self) -> Activity {
        if self.creating() {
            Activity::CreatingActor
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
        Cmd::Send(Command::CreateActor(CreateActor {
            actor_id: self.actor,
        }))
    }

    /// Follows the stream from the current offset with a new follower.
    fn refollow(&mut self) -> Cmd {
        self.follow = self.follow.next();
        self.connection = Connection::Connecting;
        Cmd::Follow {
            follow: self.follow,
            actor: self.actor,
            from: self.offset,
        }
    }

    /// Folds the followed actor's stream again, from the beginning.
    fn reload(&mut self) -> Cmd {
        self.transcript = Transcript::new(&self.limits.wire);
        self.rendered.clear();
        self.offset = Offset::BEGINNING;
        self.folded = None;
        self.scroll = None;
        self.refollow()
    }

    /// Follows the next or previous actor in the tree, if there is another.
    fn switch(&mut self, direction: Direction) -> Cmd {
        if self.creating() {
            return Cmd::None;
        }
        let order: Vec<ActorId> = view::tree(&self.actors)
            .into_iter()
            .map(|(_, actor)| actor.actor_id)
            .collect();
        let Some(here) = order.iter().position(|actor| *actor == self.actor) else {
            return Cmd::None;
        };
        let count = order.len();
        let there = match direction {
            Direction::Next => here.saturating_add(1).checked_rem(count),
            Direction::Previous => here
                .checked_add(count.saturating_sub(1))
                .and_then(|index| index.checked_rem(count)),
        };
        match there.and_then(|index| order.get(index)) {
            Some(actor) if *actor != self.actor => {
                self.actor = *actor;
                self.notice = None;
                self.reload()
            }
            Some(_) | None => Cmd::None,
        }
    }

    /// Asks the daemon for its actors, unless a listing is on its way.
    const fn list(&mut self) -> Cmd {
        if self.listing || self.creating() {
            return Cmd::None;
        }
        self.listing = true;
        Cmd::Send(Command::ListActors)
    }

    /// The tick: list the actors, and tick again.
    fn tick(&mut self) -> Cmd {
        let list = self.list();
        Cmd::Batch(vec![list, Cmd::Tick(self.limits.refresh)])
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
            actor: pending.actor,
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
            self.notice = Some("the actor is still being created".to_owned());
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
            actor: self.actor,
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
            actor: self.actor,
            turn,
        })
    }

    /// Asks to stop the actor followed; nothing is sent until `y`.
    fn ask_stop(&mut self, stamp: Stamp) -> Cmd {
        if self.creating() || self.stopping.is_some() {
            return Cmd::None;
        }
        self.stopping = Some(Stopping {
            actor: self.actor,
            stop_id: MessageId::from_parts(stamp.at, &stamp.entropy),
            confirmed: false,
        });
        self.notice = Some("stop this actor and every actor it spawned? (y/n)".to_owned());
        Cmd::None
    }

    fn confirm_stop(&mut self, confirmed: bool) -> Cmd {
        let Some(stopping) = self.stopping.filter(|stopping| !stopping.confirmed) else {
            return Cmd::None;
        };
        self.notice = None;
        if !confirmed {
            self.stopping = None;
            return Cmd::None;
        }
        self.stopping = Some(Stopping {
            confirmed: true,
            ..stopping
        });
        Cmd::Send(Command::Stop {
            actor: stopping.actor,
            body: StopActor {
                stop_id: stopping.stop_id,
            },
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
            actor: self.actor,
            body: Approve { call_id, verdict },
        })
    }

    /// Restarts what has stopped, and a follower waiting out its backoff
    /// at once; anything still on its way (a create being sent, a
    /// connection being made or live) is left alone, so holding the key
    /// starts nothing new: each restart waits for the new follower's word.
    fn reconnect(&mut self) -> Cmd {
        match (self.phase, &self.connection) {
            (Phase::Creating(Delivery::Failed), _) => {
                self.notice = None;
                self.create()
            }
            (Phase::Ready, Connection::Broken(_)) => {
                // The fold failed part-way: start over from the beginning.
                self.reload()
            }
            (Phase::Ready, Connection::Idle | Connection::Down(_) | Connection::Retrying(_)) => {
                self.refollow()
            }
            (Phase::Creating(Delivery::Sending), _)
            | (Phase::Ready, Connection::Connecting | Connection::Live(_)) => Cmd::None,
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
            Answer::ActorCreated { actor, result } => {
                if actor != self.actor || self.phase != Phase::Creating(Delivery::Sending) {
                    return Cmd::None;
                }
                match result {
                    Ok(_created_or_exists) => {
                        self.phase = Phase::Ready;
                        let follow = self.refollow();
                        Cmd::Batch(vec![follow, self.list()])
                    }
                    Err(error) => {
                        self.phase = Phase::Creating(Delivery::Failed);
                        self.notice = Some(format!(
                            "cannot create the actor: {} (^r retries)",
                            explain(&error)
                        ));
                        Cmd::None
                    }
                }
            }
            Answer::Actors(result) => {
                self.listing = false;
                // A failed listing keeps the last one: the next tick asks
                // again.
                if let Ok(list) = result {
                    self.actors = list.actors;
                }
                Cmd::None
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
            Answer::Compacted { actor, result } => {
                if actor == self.actor
                    && let Err(error) = result
                {
                    self.notice = Some(format!("cannot compact: {}", explain(&error)));
                }
                Cmd::None
            }
            Answer::Stopped { stop_id, result } => {
                if self
                    .stopping
                    .take_if(|stopping| stopping.confirmed && stopping.stop_id == stop_id)
                    .is_some()
                    && let Err(error) = result
                {
                    self.notice = Some(format!("cannot stop: {}", explain(&error)));
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
                    from == self.offset,
                    "follower {} delivered a read from {from}, but the transcript is folded up to {}",
                    follow.0,
                    self.offset
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
                self.offset = control.stream_next_offset;
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
                    Some(delay) => {
                        Connection::Retrying(format!("{error}; retrying in {}ms", delay.get()))
                    }
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

    /// The app on `start`, and its startup effects.
    pub fn init(start: Start, limits: Limits) -> (Self, Cmd) {
        let (actor, phase) = match start {
            Start::New(actor) => (actor, Phase::Creating(Delivery::Sending)),
            Start::Open(actor) => (actor, Phase::Ready),
        };
        let mut app = Self {
            limits,
            actor,
            phase,
            actors: Vec::new(),
            listing: false,
            transcript: Transcript::new(&limits.wire),
            offset: Offset::BEGINNING,
            folded: None,
            follow: FollowId(0),
            connection: Connection::Idle,
            draft: String::new(),
            pending: None,
            approving: None,
            cancelling: None,
            stopping: None,
            notice: None,
            scroll: None,
            rendered: Vec::new(),
            viewport: Viewport::default(),
        };
        let start = match phase {
            Phase::Creating(_) => app.create(),
            Phase::Ready => app.refollow(),
        };
        let tick = app.tick();
        (app, Cmd::Batch(vec![start, tick]))
    }

    /// Maps a terminal event to a message; `None` ignores it.
    #[must_use]
    pub fn on_event(&self, event: Event, stamp: Stamp) -> Option<Msg> {
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
        if is_ctrl(&key, 'k') {
            return Some(Msg::AskStop(stamp));
        }
        // A stop awaiting its `y` takes the next key.
        if self.confirming_stop() {
            return Some(Msg::ConfirmStop(matches!(
                key.code,
                KeyCode::Char('y' | 'Y')
            )));
        }
        let prompt = self.awaiting_approval().is_some();
        match key.code {
            KeyCode::Esc => Some(Msg::CancelTurn),
            KeyCode::PageUp => Some(Msg::Scroll(Scroll::PageUp)),
            KeyCode::PageDown => Some(Msg::Scroll(Scroll::PageDown)),
            KeyCode::Tab => Some(Msg::Switch(Direction::Next)),
            KeyCode::BackTab => Some(Msg::Switch(Direction::Previous)),
            // The approval prompt answers `y` and `n` while nothing is
            // typed. Anything else is typed into the draft, and Enter on
            // it steers the turn: the calls waiting are denied.
            KeyCode::Char('y' | 'Y') if prompt && self.draft.is_empty() => {
                Some(Msg::Decide(Verdict::Grant))
            }
            KeyCode::Char('n' | 'N') if prompt && self.draft.is_empty() => {
                Some(Msg::Decide(Verdict::Deny))
            }
            KeyCode::Enter if prompt && self.draft.is_empty() => None,
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

    /// Applies a message, returning follow-up effects. Pure.
    pub fn update(&mut self, msg: Msg) -> Cmd {
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
            Msg::AskStop(stamp) => self.ask_stop(stamp),
            Msg::ConfirmStop(confirmed) => self.confirm_stop(confirmed),
            Msg::Decide(verdict) => self.decide(verdict),
            Msg::Switch(direction) => self.switch(direction),
            Msg::Scroll(scroll) => {
                self.scroll(scroll);
                Cmd::None
            }
            Msg::Reconnect => self.reconnect(),
            Msg::Quit => Cmd::Quit,
            Msg::Feedback(Feedback::Answered(answer)) => self.answered(answer),
            Msg::Feedback(Feedback::Tick) => self.tick(),
            Msg::Feedback(Feedback::Stream { follow, update }) => self.streamed(follow, update),
        }
    }

    /// Draws the app. It may update render caches (never state `update`
    /// reads), hence `&mut self`.
    pub fn view(&mut self, frame: &mut Frame<'_>) {
        let draft_lines = u16::try_from(self.draft.split('\n').count()).unwrap_or(u16::MAX);
        let input_height = draft_lines
            .clamp(1, self.limits.input_lines.max(1))
            .saturating_add(2);
        let name = self
            .actors
            .iter()
            .find(|actor| actor.actor_id == self.actor)
            .map(|actor| actor.name.clone());
        let prompt = self
            .awaiting_approval()
            .map(|tool| view::approval_line(name.as_deref(), tool));
        let prompt_height = u16::from(prompt.is_some());
        let tree = if self.actors.len() > 1 {
            view::tree_lines(&self.actors, self.actor, self.limits.tree_lines)
        } else {
            Vec::new()
        };
        let tasks = view::task_lines(self.transcript.tasks(), self.limits.task_lines);
        let [tree_area, main, tasks_area, prompt_area, input, footer] = Layout::vertical([
            Constraint::Length(u16::try_from(tree.len()).unwrap_or(u16::MAX)),
            Constraint::Fill(1),
            Constraint::Length(u16::try_from(tasks.len()).unwrap_or(u16::MAX)),
            Constraint::Length(prompt_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(Paragraph::new(tree), tree_area);
        frame.render_widget(Paragraph::new(tasks), tasks_area);

        let mut lines = self.transcript_lines();
        lines.extend(view::queued_lines(self.transcript.queued()));
        let transcript = Paragraph::new(lines)
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

        let actor = self.actor.to_string();
        let short = actor.get(actor.len().saturating_sub(8)..).unwrap_or(&actor);
        let model = self
            .transcript
            .turns()
            .last()
            .map(verified_wire::transcript::TranscriptTurn::model);
        let status = Status {
            name: name.as_deref(),
            actor: short,
            model,
            connection: &self.connection,
            activity: self.activity(),
            notice: self.notice.as_deref(),
        };
        frame.render_widget(Paragraph::new(view::status_line(&status)), footer);
    }
}

/// What to tell the user about a refusal. The match is exhaustive on
/// purpose: each new kind of refusal gets its own thought here.
fn refusal(rejection: &Rejection) -> String {
    let hint = match rejection {
        Rejection::UnknownActor { .. }
        | Rejection::ActorFull { .. }
        | Rejection::ActorStopped { .. } => Some("restart verified for a new session"),
        Rejection::TooManyActors { .. } => Some("close another session's client"),
        Rejection::TooManyFollowers { .. } => Some("close another client of this actor"),
        Rejection::Malformed { .. }
        | Rejection::MessageExistsWithDifferentText { .. }
        | Rejection::NoPendingApproval { .. }
        | Rejection::ApprovalExistsWithDifferentVerdict { .. }
        | Rejection::TurnNotRunning { .. }
        | Rejection::TurnRunning { .. } => None,
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

#[cfg(test)]
mod tests;
