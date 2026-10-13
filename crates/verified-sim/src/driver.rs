//! The TUI host: the real TUI (`verified_tui` with its `sim` shim) on the
//! simulated network, typed at by a scripted user through the Terminal seam.
//!
//! The user reads the app after every redraw (a watch channel fed by the
//! TUI's `observe` hook) and reacts like a person would: sends the next
//! message once the last turn is over, answers approval prompts, presses Esc
//! when the plan says to, resends a message that did not get through,
//! reconnects when the TUI says it stopped following, and gives up the rest
//! of the plan when the TUI says the session takes no more.

use std::collections::BTreeMap;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers};
use tokio::sync::watch;
use verified_domain::Millis;
use verified_tui::app::{App, Start};
use verified_tui::seam::{Clock as _, TcpDialer};
use verified_tui::tea::Seams;
use verified_tui::terminal::{Keyboard, KeyboardError, TestScreen, scripted};
use verified_tui::view::Connection;
use verified_tui::{Limits, new_session};
use verified_wire::command::Verdict;
use verified_wire::transcript::{Part, ToolState};

use crate::clock::SimClock;
use crate::entropy::SeededEntropy;
use crate::hosts::{DAEMON, PORT, epoch};
use crate::ledger::{Entry, Ledger};
use crate::scenario::{CancelAt, TurnPlan};

/// How many keys may wait unread.
const KEYS: usize = 64;
/// How often the user looks again when nothing changed.
const GLANCE: Millis = Millis::new(100);
/// Mixed into the seed for the TUI's entropy.
const TUI_SALT: u64 = 0x7475_6900;
/// What the TUI's notice says when the session takes no more commands
/// (it is full, or gone): a person would stop there.
const OVER: &str = "restart verified for a new session";

/// What the user sees after a redraw.
#[derive(Debug, Clone, Default, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a snapshot of the app's yes/no observers, one field each, for the scripted user to read"
)]
struct Seen {
    turns: usize,
    running: bool,
    running_text: bool,
    tool_running: bool,
    approval: Option<String>,
    caught_up: bool,
    settled: bool,
    creating: bool,
    noticed: bool,
    delivery_failed: bool,
    stopped: bool,
    /// The TUI says the session takes no more ([`OVER`]).
    over: bool,
    draft: bool,
    offset: u64,
    /// The folded transcript, when nothing is in flight.
    transcript: Option<serde_json::Value>,
}

impl Seen {
    fn of(app: &App) -> Self {
        let running = app.transcript().running();
        let reply = running.and_then(verified_wire::transcript::TranscriptTurn::reply);
        let parts: &[Part] = reply.as_ref().map_or(&[], |reply| reply.parts);
        let running_text = parts
            .iter()
            .any(|part| matches!(part, Part::Text { text, .. } if !text.is_empty()));
        let tool_running = parts.iter().any(
            |part| matches!(part, Part::Tool(tool) if tool.state == ToolState::ApprovalResponded),
        );
        let quiet = app.caught_up() && app.settled() && running.is_none() && !app.creating();
        Self {
            turns: app.transcript().turns().len(),
            running: running.is_some(),
            running_text,
            tool_running,
            approval: app
                .awaiting_approval()
                .map(|tool| tool.tool_call_id.to_string()),
            caught_up: app.caught_up(),
            settled: app.settled(),
            creating: app.creating(),
            noticed: app.notice().is_some(),
            delivery_failed: app.delivery_failed(),
            stopped: matches!(
                app.connection(),
                Connection::Down(_) | Connection::Broken(_)
            ),
            over: app.notice().is_some_and(|notice| notice.contains(OVER)),
            draft: !app.draft().is_empty(),
            offset: app.offset().seq(),
            transcript: quiet
                .then(|| serde_json::to_value(app.transcript().turns()).ok())
                .flatten(),
        }
    }

    const fn quiet(&self) -> bool {
        self.caught_up && self.settled && !self.running && !self.creating
    }
}

/// Why the scripted user stopped early.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum ScriptError {
    #[error("the TUI stopped reading keys")]
    Keyboard(#[from] KeyboardError),
    #[error("the TUI stopped before the script was done")]
    Stopped,
}

/// The scripted user.
struct User {
    keyboard: Keyboard,
    seen: watch::Receiver<Seen>,
    clock: SimClock,
    ledger: Ledger,
    /// The answer given to each prompt, so a resent answer never flips.
    verdicts: BTreeMap<String, Verdict>,
}

impl User {
    async fn press(&self, event: crossterm::event::Event) -> Result<(), ScriptError> {
        Ok(self.keyboard.send(event).await?)
    }

    async fn ctrl(&self, letter: char) -> Result<(), ScriptError> {
        self.press(Keyboard::key(KeyCode::Char(letter), KeyModifiers::CONTROL))
            .await
    }

    /// Waits for a redraw, or a glance's time.
    async fn glance(&mut self) -> Result<Seen, ScriptError> {
        tokio::select! {
            biased;
            changed = self.seen.changed() => changed.map_err(|_closed| ScriptError::Stopped)?,
            () = self.clock.sleep(GLANCE) => {}
        }
        Ok(self.seen.borrow_and_update().clone())
    }

    /// What anyone would do whatever the plan: retry what did not get
    /// through and reconnect what stopped.
    async fn unstick(&self, seen: &Seen) -> Result<(), ScriptError> {
        if seen.delivery_failed {
            self.ledger.record(Entry::User("resend".to_owned()));
            self.press(Keyboard::enter()).await?;
        } else if seen.creating && seen.noticed {
            self.ledger.record(Entry::User("retry creating".to_owned()));
            self.ctrl('r').await?;
        } else if seen.stopped {
            self.ledger.record(Entry::User("reconnect".to_owned()));
            self.ctrl('r').await?;
        }
        Ok(())
    }

    /// Plays turn `index` of the plan; `false` when the TUI says the
    /// session takes no more.
    async fn play_turn(&mut self, index: usize, turn: &TurnPlan) -> Result<bool, ScriptError> {
        loop {
            let seen = self.glance().await?;
            if seen.quiet() && seen.turns == index && !seen.draft {
                break;
            }
            self.unstick(&seen).await?;
        }
        self.ledger
            .record(Entry::User(format!("send {index}: {}", turn.text)));
        self.press(Keyboard::paste(turn.text.clone())).await?;
        self.press(Keyboard::enter()).await?;
        let mut cancelled = false;
        let mut prompts = 0usize;
        loop {
            let seen = self.glance().await?;
            if seen.over {
                return Ok(false);
            }
            if seen.turns > index && !seen.running && seen.settled {
                return Ok(true);
            }
            self.unstick(&seen).await?;
            if seen.draft && seen.settled && !seen.running && seen.caught_up {
                // The daemon refused the message for good; send it again.
                self.ledger
                    .record(Entry::User("send again after a refusal".to_owned()));
                self.press(Keyboard::enter()).await?;
                continue;
            }
            let cancel = match turn.cancel {
                Some(CancelAt::Streaming) => seen.running_text,
                Some(CancelAt::ToolRunning) => seen.tool_running,
                Some(CancelAt::Prompt) => seen.approval.is_some(),
                None => false,
            };
            if cancel && !cancelled && seen.running {
                cancelled = true;
                self.ledger.record(Entry::User(format!("cancel {index}")));
                self.press(Keyboard::key(KeyCode::Esc, KeyModifiers::NONE))
                    .await?;
                continue;
            }
            let Some(call) = seen.approval.clone() else {
                continue;
            };
            if !seen.settled || (cancelled && turn.cancel == Some(CancelAt::Prompt)) {
                continue;
            }
            let verdict = if let Some(verdict) = self.verdicts.get(&call) {
                *verdict
            } else {
                let verdict = turn
                    .verdicts
                    .get(prompts.checked_rem(turn.verdicts.len()).unwrap_or(0))
                    .copied()
                    .unwrap_or(Verdict::Grant);
                prompts = prompts.saturating_add(1);
                self.verdicts.insert(call.clone(), verdict);
                verdict
            };
            self.ledger
                .record(Entry::User(format!("answer {call}: {verdict:?}")));
            let letter = match verdict {
                Verdict::Grant => 'y',
                Verdict::Deny => 'n',
            };
            self.press(Keyboard::key(KeyCode::Char(letter), KeyModifiers::NONE))
                .await?;
        }
    }

    async fn play(mut self, plan: &[TurnPlan]) -> Result<(), ScriptError> {
        let mut over = false;
        for (index, turn) in plan.iter().enumerate() {
            if !self.play_turn(index, turn).await? {
                self.ledger.record(Entry::GaveUp { turn: index });
                over = true;
                break;
            }
        }
        loop {
            let seen = self.glance().await?;
            // Caught up for real: what the TUI folded is everything the
            // daemon acknowledged (a TUI still "live" on a connection to a
            // crashed daemon learns otherwise only at its idle timeout).
            let current = seen.offset == self.ledger.logged();
            if let (true, true, Some(transcript)) =
                (over || seen.turns == plan.len(), current, seen.transcript)
            {
                self.ledger.record(Entry::Caught {
                    offset: seen.offset,
                    transcript,
                });
                break;
            }
            let seen = self.seen.borrow().clone();
            self.unstick(&seen).await?;
        }
        self.ledger.record(Entry::User("quit".to_owned()));
        self.press(Keyboard::quit()).await
    }
}

/// The TUI host: creates a session, plays `plan`, quits once quiescent.
///
/// # Errors
/// The TUI failed, or stopped before the script was done.
///
/// # Panics
/// The observer missed a folded read (a harness defect: it sees every update).
pub async fn tui(plan: Vec<TurnPlan>, seed: u64, ledger: Ledger) -> turmoil::Result {
    let clock = SimClock::new(epoch());
    let entropy = SeededEntropy::new(seed ^ TUI_SALT);
    let session = new_session(&clock, &entropy);
    let (keyboard, input) = scripted(KEYS);
    let seams = Seams {
        input: Box::new(input),
        dialer: Arc::new(TcpDialer::new(DAEMON.to_owned(), PORT)),
        clock: Arc::new(clock),
        entropy: Box::new(entropy),
    };
    let (shown, watching) = watch::channel(Seen::default());
    let mut screen = TestScreen::new(100, 30);
    // The observer sees the app after every update, so it sees every read
    // the app folds, one at a time; each goes to the ledger for I6.
    let reads = ledger.clone();
    let mut folded: u64 = 0;
    let mut observe = |app: &App| {
        if let Some(read) = app.last_read()
            && read.count != folded
        {
            assert!(
                read.count == folded.saturating_add(1),
                "the observer missed a read: saw {folded}, now {}",
                read.count
            );
            folded = read.count;
            reads.record(Entry::Read {
                from: read.from.seq(),
                next: read.next.seq(),
                chunks: read.chunks.clone(),
            });
        }
        shown.send_replace(Seen::of(app));
    };
    let user = User {
        keyboard,
        seen: watching,
        clock,
        ledger,
        verdicts: BTreeMap::new(),
    };
    let running = verified_tui::run(
        Start::New(session),
        Limits::DEFAULT,
        seams,
        &mut screen,
        &mut observe,
    );
    let playing = user.play(&plan);
    let (app, played) = tokio::join!(running, playing);
    app?;
    played?;
    Ok(())
}
