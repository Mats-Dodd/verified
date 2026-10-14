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
use verified_domain::Address;
use verified_domain::Millis;
use verified_domain::id::UnixMillis;
use verified_tui::app::{App, Start};
use verified_tui::seam::{Clock as _, TcpDialer};
use verified_tui::tea::Seams;
use verified_tui::terminal::{Keyboard, KeyboardError, TestScreen, scripted};
use verified_tui::view::Connection;
use verified_tui::{Limits, new_session};
use verified_wire::command::{ActorState, Verdict};
use verified_wire::transcript::{Mail, Part, ToolState, TranscriptTurn};

use crate::clock::SimClock;
use crate::entropy::SeededEntropy;
use crate::hosts::{DAEMON, PORT, epoch};
use crate::ledger::{Entry, Ledger};
use crate::scenario::{CancelAt, SteerAt, TurnPlan};

/// How many keys may wait unread.
const KEYS: usize = 64;
/// How often the user looks again when nothing changed.
const GLANCE: Millis = Millis::new(100);
/// Glances a running turn may go without a new record, and no prompt, before
/// the user presses Esc: three simulated minutes, past any tool's timeout.
/// A turn stalls only when its log is full and the session failed stop
/// (L3); Esc then learns the session takes no more.
const STALL: usize = 1_800;
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
    /// The user's messages the root holds: carried by a step, or queued.
    delivered: usize,
    /// Every other actor, as last listed, is quiet: no turn running, no
    /// approval waiting, no task running.
    others_quiet: bool,
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
        let reply = running.and_then(TranscriptTurn::reply);
        let parts: &[Part] = reply.as_ref().map_or(&[], |reply| reply.parts);
        let running_text = parts
            .iter()
            .any(|part| matches!(part, Part::Text { text, .. } if !text.is_empty()));
        let tool_running = parts.iter().any(
            |part| matches!(part, Part::Tool(tool) if tool.state == ToolState::ApprovalResponded),
        );
        let over = app.notice().is_some_and(|notice| notice.contains(OVER));
        // A session that takes no more may leave its turn running for good
        // (a full log fails stop): what it folded is then final too.
        let quiet =
            app.caught_up() && app.settled() && (running.is_none() || over) && !app.creating();
        let user = |mail: &Mail| mail.from == Address::User;
        let delivered = app
            .transcript()
            .turns()
            .iter()
            .flat_map(TranscriptTurn::inputs)
            .filter(|input| user(&input.mail))
            .count()
            .saturating_add(
                app.transcript()
                    .queued()
                    .iter()
                    .filter(|mail| user(mail))
                    .count(),
            );
        let others_quiet = app.actors().iter().all(|actor| {
            actor.actor_id == app.actor()
                || (!matches!(actor.state, ActorState::Running { .. })
                    && actor.approvals.is_empty()
                    && actor.tasks == 0)
        });
        Self {
            delivered,
            others_quiet,
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
            over,
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

/// How long every actor must look quiet before the user believes it: past
/// two refreshes of the actor list, so a listing taken before the last
/// change cannot fool it.
const QUIET_FOR: Millis = Millis::new(2_500);

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

    /// Plays turn `index` of the plan, the `typed` message the user sends;
    /// `false` when the TUI says the session takes no more.
    async fn play_turn(
        &mut self,
        (index, typed): (usize, &mut usize),
        turn: &TurnPlan,
    ) -> Result<bool, ScriptError> {
        loop {
            let seen = self.glance().await?;
            if seen.quiet() && seen.delivered == *typed && !seen.draft {
                break;
            }
            self.unstick(&seen).await?;
        }
        self.ledger
            .record(Entry::User(format!("send {index}: {}", turn.text)));
        self.press(Keyboard::paste(turn.text.clone())).await?;
        self.press(Keyboard::enter()).await?;
        *typed = typed.saturating_add(1);
        let mut cancelled = false;
        let mut steered = turn.steer.is_none();
        // The prompt a steer answered instead of the user: its call is
        // denied, so no verdict is sent for it.
        let mut steered_past: Option<String> = None;
        let mut prompts = 0usize;
        let mut last = u64::MAX;
        let mut still = 0usize;
        loop {
            let seen = self.glance().await?;
            if seen.over {
                return Ok(false);
            }
            still = if seen.offset == last && seen.approval.is_none() {
                still.saturating_add(1)
            } else {
                0
            };
            last = seen.offset;
            if seen.running && !cancelled && still >= STALL {
                cancelled = true;
                self.ledger
                    .record(Entry::User(format!("cancel {index}: stalled")));
                self.press(Keyboard::key(KeyCode::Esc, KeyModifiers::NONE))
                    .await?;
                continue;
            }
            if seen.delivered >= *typed && !seen.running && seen.settled {
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
            if let Some(steer) = turn.steer.as_ref().filter(|_| !steered)
                && seen.running
                && seen.settled
                && !seen.draft
                && match steer.at {
                    SteerAt::Streaming => seen.running_text,
                    SteerAt::ToolRunning => seen.tool_running,
                    SteerAt::Prompt => seen.approval.is_some(),
                }
            {
                steered = true;
                if steer.at == SteerAt::Prompt {
                    steered_past.clone_from(&seen.approval);
                }
                self.ledger
                    .record(Entry::User(format!("steer {index}: {}", steer.text)));
                self.press(Keyboard::paste(steer.text.clone())).await?;
                self.press(Keyboard::enter()).await?;
                *typed = typed.saturating_add(1);
                continue;
            }
            let cancel = match turn.cancel {
                Some(CancelAt::Streaming) => seen.running_text,
                Some(CancelAt::ToolRunning) => seen.tool_running,
                Some(CancelAt::Prompt) => seen.approval.is_some(),
                None => false,
            };
            // After a steer, Esc waits for it to be durable: it then
            // lands before the cancel, injected into the turn.
            if cancel && !cancelled && seen.running && (turn.steer.is_none() || seen.settled) {
                cancelled = true;
                self.ledger.record(Entry::User(format!("cancel {index}")));
                self.press(Keyboard::key(KeyCode::Esc, KeyModifiers::NONE))
                    .await?;
                continue;
            }
            let Some(call) = seen
                .approval
                .clone()
                .filter(|call| steered_past.as_ref() != Some(call))
            else {
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

    /// Stops the root (Ctrl-K, then `y`) once it is quiet, and waits for
    /// the daemon's answer.
    async fn stop(&mut self) -> Result<(), ScriptError> {
        loop {
            let seen = self.glance().await?;
            if seen.quiet() && !seen.draft {
                break;
            }
            self.unstick(&seen).await?;
        }
        self.ledger.record(Entry::User("stop".to_owned()));
        self.ctrl('k').await?;
        self.press(Keyboard::key(KeyCode::Char('y'), KeyModifiers::NONE))
            .await?;
        loop {
            let seen = self.glance().await?;
            if seen.settled {
                return Ok(());
            }
            self.unstick(&seen).await?;
        }
    }

    async fn play(mut self, plan: &Plan) -> Result<(), ScriptError> {
        let mut over = false;
        let mut typed = 0usize;
        for (index, turn) in plan.turns.iter().enumerate() {
            if !self.play_turn((index, &mut typed), turn).await? {
                self.ledger.record(Entry::GaveUp { turn: index });
                over = true;
                break;
            }
        }
        if plan.stop && !over {
            self.stop().await?;
        }
        let mut quiet_since: Option<UnixMillis> = None;
        loop {
            let seen = self.glance().await?;
            // Caught up for real: what the TUI folded is everything the
            // daemon acknowledged (a TUI still "live" on a connection to a
            // crashed daemon learns otherwise only at its idle timeout).
            let current = self
                .ledger
                .root()
                .is_some_and(|root| seen.offset == self.ledger.logged(root));
            let now = self.clock.now();
            quiet_since = if seen.others_quiet && seen.transcript.is_some() {
                quiet_since.or(Some(now))
            } else {
                None
            };
            let settled = quiet_since
                .is_some_and(|since| now.get().saturating_sub(since.get()) >= QUIET_FOR.get());
            if let (true, true, true, Some(transcript)) = (
                over || seen.delivered >= typed,
                current,
                settled,
                seen.transcript,
            ) {
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

/// What the scripted user does: its turns, then a stop of the root if
/// `stop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub turns: Vec<TurnPlan>,
    pub stop: bool,
}

/// The TUI host: creates a session, plays `plan`, quits once quiescent.
///
/// # Errors
/// The TUI failed, or stopped before the script was done.
///
/// # Panics
/// The observer missed a folded read (a harness defect: it sees every update).
pub async fn tui(plan: Plan, seed: u64, ledger: Ledger) -> turmoil::Result {
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
