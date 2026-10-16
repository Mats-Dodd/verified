//! Terminals behind the [`Input`] and [`Screen`] seams: the real one
//! (crossterm) and a scripted one (a [`TestBackend`] and keys sent over a
//! bounded channel) for tests and simulation.

use core::pin::Pin;
use std::io;

use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyModifiers,
};
use futures_core::Stream as _;
use ratatui::backend::TestBackend;
use ratatui::{DefaultTerminal, Frame, Terminal};
use tokio::sync::mpsc;

use crate::seam::{Input, Screen, TerminalError};

/// The real terminal's output: raw mode, alternate screen, bracketed paste.
pub struct CrosstermScreen {
    terminal: DefaultTerminal,
}

impl core::fmt::Debug for CrosstermScreen {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CrosstermScreen")
            .finish_non_exhaustive()
    }
}

impl CrosstermScreen {
    /// Takes over the terminal. Call [`CrosstermScreen::leave`] to give it back.
    ///
    /// # Errors
    /// The terminal could not be put in raw mode or the alternate screen.
    pub fn enter() -> Result<Self, TerminalError> {
        let terminal = ratatui::try_init()?;
        crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(Self { terminal })
    }

    /// Restores the terminal.
    ///
    /// # Errors
    /// The terminal could not be restored.
    pub fn leave(self) -> Result<(), TerminalError> {
        crossterm::execute!(io::stdout(), DisableBracketedPaste)?;
        ratatui::try_restore()?;
        Ok(())
    }
}

impl Screen for CrosstermScreen {
    fn draw(&mut self, view: &mut dyn FnMut(&mut Frame<'_>)) -> Result<(), TerminalError> {
        self.terminal.draw(|frame| view(frame))?;
        Ok(())
    }
}

/// The real terminal's input.
pub struct CrosstermInput {
    events: EventStream,
}

impl core::fmt::Debug for CrosstermInput {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CrosstermInput")
            .finish_non_exhaustive()
    }
}

impl CrosstermInput {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: EventStream::new(),
        }
    }
}

impl Default for CrosstermInput {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Input for CrosstermInput {
    async fn next_event(&mut self) -> Result<Option<Event>, TerminalError> {
        // EventStream is cancel-safe: an event is only taken when returned.
        let next = core::future::poll_fn(|cx| Pin::new(&mut self.events).poll_next(cx)).await;
        next.transpose().map_err(TerminalError)
    }
}

/// Scripted input: events arrive from a [`Keyboard`].
#[derive(Debug)]
pub struct ScriptedInput {
    events: mpsc::Receiver<Event>,
}

/// Sends scripted events to a [`ScriptedInput`]. Dropping every keyboard
/// ends the input, which ends the program.
#[derive(Debug, Clone)]
pub struct Keyboard {
    events: mpsc::Sender<Event>,
}

/// A scripted input and the keyboard that types into it; at most `capacity`
/// events wait unread.
#[must_use]
pub fn scripted(capacity: usize) -> (Keyboard, ScriptedInput) {
    let (events, receiver) = mpsc::channel(capacity);
    (Keyboard { events }, ScriptedInput { events: receiver })
}

#[async_trait]
impl Input for ScriptedInput {
    async fn next_event(&mut self) -> Result<Option<Event>, TerminalError> {
        Ok(self.events.recv().await)
    }
}

/// Why a scripted event was not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyboardError {
    #[error("the program has stopped reading input")]
    Closed,
    #[error("the input queue is full")]
    Full,
}

impl Keyboard {
    /// Sends `event`, waiting while the queue is full.
    ///
    /// # Errors
    /// The program has stopped reading input.
    pub async fn send(&self, event: Event) -> Result<(), KeyboardError> {
        self.events
            .send(event)
            .await
            .map_err(|_closed| KeyboardError::Closed)
    }

    /// Sends `event` now, from synchronous code (such as an observer).
    ///
    /// # Errors
    /// The queue is full, or the program has stopped reading input.
    pub fn try_send(&self, event: Event) -> Result<(), KeyboardError> {
        self.events.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => KeyboardError::Full,
            mpsc::error::TrySendError::Closed(_) => KeyboardError::Closed,
        })
    }

    /// A key press event.
    #[must_use]
    pub const fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    /// Ctrl-C: quits.
    #[must_use]
    pub const fn quit() -> Event {
        Self::key(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// Text arriving as one paste.
    #[must_use]
    pub const fn paste(text: String) -> Event {
        Event::Paste(text)
    }

    /// Enter: sends the message.
    #[must_use]
    pub const fn enter() -> Event {
        Self::key(KeyCode::Enter, KeyModifiers::NONE)
    }
}

/// An in-memory screen whose contents can be read back as text.
#[derive(Debug)]
pub struct TestScreen {
    terminal: Terminal<TestBackend>,
}

impl TestScreen {
    /// # Panics
    /// Never: a test backend cannot fail.
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        let terminal =
            Terminal::new(TestBackend::new(width, height)).expect("a test backend never fails");
        Self { terminal }
    }

    /// The last frame, one string per row, trailing spaces trimmed.
    #[must_use]
    pub fn text(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        let width = usize::from(buffer.area.width).max(1);
        let mut out = String::new();
        for row in buffer.content.chunks(width) {
            let line: String = row.iter().map(ratatui::buffer::Cell::symbol).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

impl Screen for TestScreen {
    fn draw(&mut self, view: &mut dyn FnMut(&mut Frame<'_>)) -> Result<(), TerminalError> {
        match self.terminal.draw(|frame| view(frame)) {
            Ok(_frame) => Ok(()),
            Err(never) => match never {},
        }
    }
}
