//! The Elm runtime: messages in, commands as data out.
//!
//! Ported from q's `tea.rs`, made async and with [`Cmd`] as plain data.
//! [`App::update`] is pure and never touches the network or the terminal: it
//! returns a [`Cmd`] describing what to do, and the runtime, which owns every
//! effect, does it. Results come back into the one bounded inbox as
//! [`Feedback`], alongside terminal input. The loop waits for the first
//! message, drains whatever else is already queued, and redraws once.
//!
//! Effects run as tasks in one `JoinSet` owned by the runtime. They stop at
//! safe points through cancellation tokens, never by being aborted. A
//! panicking task panics the runtime (crash-only).

use std::sync::Arc;

use crossterm::event::Event;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use verified_domain::id::{IdEntropy, SessionId, UnixMillis};

use crate::Limits;
use crate::app::{App, Msg, Start};
use crate::client::{Answer, Client, Command, Position, StreamUpdate};
use crate::seam::{Clock, Dialer, Entropy, Input, Screen, TerminalError};

/// When and with what randomness an input event arrived: everything
/// [`App::update`] needs to mint ids without a clock or a random source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub at: UnixMillis,
    pub entropy: IdEntropy,
}

/// Which follower an update came from. The app picks a new one whenever it
/// (re)starts following, so updates from a replaced follower can be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FollowId(pub u64);

impl FollowId {
    /// The follower after this one.
    ///
    /// # Panics
    /// After `u64::MAX` followers, which no run reaches.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("fewer than u64::MAX followers"),
        )
    }
}

/// An effect requested by [`App::update`], done by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a command does nothing until returned to the runtime"]
pub enum Cmd {
    /// Nothing.
    None,
    /// Send a command to the daemon; the answer arrives as [`Feedback::Answered`].
    Send(Command),
    /// Follow a session's stream, replacing any current follower.
    Follow {
        follow: FollowId,
        session: SessionId,
        from: Position,
    },
    /// Stop the current follower.
    StopFollowing,
    /// Several effects, in order.
    Batch(Vec<Self>),
    /// Stop the program.
    Quit,
}

/// What effects report back.
#[derive(Debug)]
pub enum Feedback {
    Answered(Answer),
    Stream {
        follow: FollowId,
        update: StreamUpdate,
    },
}

/// Everything the runtime needs from outside.
#[derive(Debug)]
pub struct Seams {
    pub input: Box<dyn Input>,
    pub dialer: Arc<dyn Dialer>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Box<dyn Entropy>,
}

/// Why the program stopped other than by quitting.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Terminal(#[from] TerminalError),
}

/// Everything that can wake the loop.
enum Incoming {
    Event(Event),
    Feedback(Feedback),
    InputEnded,
    InputFailed(TerminalError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// The effects side of the loop.
struct Effects {
    client: Client,
    inbox: mpsc::Sender<Incoming>,
    tasks: JoinSet<()>,
    root: CancellationToken,
    follower: Option<CancellationToken>,
    limits: Limits,
}

impl Effects {
    fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        assert!(
            self.tasks.len() < self.limits.tasks,
            "at most {} effect tasks run at once; the app asked for more",
            self.limits.tasks
        );
        self.tasks.spawn(task);
    }

    fn exec(&mut self, cmd: Cmd) -> Flow {
        match cmd {
            Cmd::None => Flow::Continue,
            Cmd::Send(command) => {
                let (client, inbox, stop) =
                    (self.client.clone(), self.inbox.clone(), self.root.clone());
                self.spawn(async move {
                    // On shutdown the request is abandoned: the daemon dedups
                    // the id if it landed, and nobody is left to tell.
                    let answer = tokio::select! {
                        biased;
                        () = stop.cancelled() => return,
                        answer = client.send(command) => answer,
                    };
                    let feedback = Feedback::Answered(answer);
                    let _delivered = inbox.send(Incoming::Feedback(feedback)).await;
                });
                Flow::Continue
            }
            Cmd::Follow {
                follow,
                session,
                from,
            } => {
                if let Some(previous) = self.follower.take() {
                    previous.cancel();
                }
                let stop = self.root.child_token();
                self.follower = Some(stop.clone());
                let (client, inbox) = (self.client.clone(), self.inbox.clone());
                self.spawn(async move {
                    let wrap =
                        move |update| Incoming::Feedback(Feedback::Stream { follow, update });
                    client.follow(session, from, &inbox, wrap, &stop).await;
                });
                Flow::Continue
            }
            Cmd::StopFollowing => {
                if let Some(previous) = self.follower.take() {
                    previous.cancel();
                }
                Flow::Continue
            }
            Cmd::Batch(cmds) => {
                for cmd in cmds {
                    if self.exec(cmd) == Flow::Quit {
                        return Flow::Quit;
                    }
                }
                Flow::Continue
            }
            Cmd::Quit => Flow::Quit,
        }
    }
}

/// Resumes a task's panic here; tasks are never aborted.
fn reap(joined: Result<(), JoinError>) {
    if let Err(error) = joined {
        assert!(error.is_panic(), "effect tasks are never aborted: {error}");
        std::panic::resume_unwind(error.into_panic());
    }
}

/// Forwards terminal input to the inbox until shutdown or the input ends.
async fn read_input(
    mut input: Box<dyn Input>,
    inbox: mpsc::Sender<Incoming>,
    stop: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            event = input.next_event() => event,
        };
        let (incoming, flow) = match event {
            Ok(Some(event)) => (Incoming::Event(event), Flow::Continue),
            Ok(None) => (Incoming::InputEnded, Flow::Quit),
            Err(error) => (Incoming::InputFailed(error), Flow::Quit),
        };
        if inbox.send(incoming).await.is_err() || flow == Flow::Quit {
            return;
        }
    }
}

/// Runs the app on `start` until the user quits or input ends, and returns
/// the final app (its transcript is the fold of what the stream delivered).
///
/// `observe` sees the app after every redraw and after every update (several
/// updates may share one redraw); simulation uses it to check each folded
/// read and the transcript, and to script input in reaction to them.
///
/// # Errors
/// Drawing to or reading from the terminal failed.
///
/// # Panics
/// When an effect task panics (crash-only), or the app starts more than
/// [`Limits::tasks`] effects at once.
pub async fn run(
    start: Start,
    limits: Limits,
    seams: Seams,
    screen: &mut dyn Screen,
    observe: &mut dyn FnMut(&App),
) -> Result<App, RunError> {
    let Seams {
        input,
        dialer,
        clock,
        mut entropy,
    } = seams;
    let (inbox, mut mailbox) = mpsc::channel::<Incoming>(limits.inbox);
    let mut effects = Effects {
        client: Client::new(dialer, Arc::clone(&clock), limits),
        inbox,
        tasks: JoinSet::new(),
        root: CancellationToken::new(),
        follower: None,
        limits,
    };
    let input_stop = effects.root.child_token();
    let input_inbox = effects.inbox.clone();
    effects.spawn(read_input(input, input_inbox, input_stop));

    let (mut app, cmd) = App::init(start, limits);
    let mut flow = effects.exec(cmd);
    let mut outcome = Ok(());
    while flow == Flow::Continue {
        if let Err(error) = screen.draw(&mut |frame| app.view(frame)) {
            outcome = Err(RunError::Terminal(error));
            break;
        }
        observe(&app);

        let first = tokio::select! {
            biased;
            incoming = mailbox.recv() => incoming,
            Some(joined) = effects.tasks.join_next() => {
                reap(joined);
                continue;
            }
        };
        let mut next = Some(first.expect("the runtime holds a sender, so the inbox stays open"));
        let mut drained: usize = 0;
        while let Some(incoming) = next {
            let msg = match incoming {
                Incoming::Event(event) => {
                    let stamp = Stamp {
                        at: clock.now(),
                        entropy: entropy.id_entropy(),
                    };
                    app.on_event(event, stamp)
                }
                Incoming::Feedback(feedback) => Some(Msg::Feedback(feedback)),
                Incoming::InputEnded => {
                    flow = Flow::Quit;
                    None
                }
                Incoming::InputFailed(error) => {
                    outcome = Err(RunError::Terminal(error));
                    flow = Flow::Quit;
                    None
                }
            };
            if let Some(msg) = msg {
                flow = effects.exec(app.update(msg));
                observe(&app);
            }
            if flow == Flow::Quit {
                break;
            }
            drained = drained.saturating_add(1);
            next = if drained < limits.inbox {
                mailbox.try_recv().ok()
            } else {
                None
            };
        }
    }

    // Shutdown: stop every effect at its next safe point, refuse further
    // messages, and wait for every task to finish.
    effects.root.cancel();
    mailbox.close();
    while let Some(joined) = effects.tasks.join_next().await {
        reap(joined);
    }
    outcome.map(|()| app)
}
