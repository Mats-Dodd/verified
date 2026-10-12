//! The Elm runtime: messages in, commands as data out.
//!
//! Ported from q's `tea.rs`, made async and with [`Cmd`] as plain data.
//! `update` is pure and never touches the network or the terminal: it
//! returns a [`Cmd`] describing what to do, and the runtime, which owns every
//! effect, does it. Results come back into the one bounded inbox as
//! [`Feedback`], alongside terminal input. The loop waits for the first
//! message, drains whatever else is already queued, and redraws once.
//!
//! Effects run as tasks in one `JoinSet` owned by the runtime (the TUI's
//! world layer). They stop at safe points through cancellation tokens, never
//! by being aborted. A panicking task panics the runtime (crash-only).

use std::sync::Arc;

use crossterm::event::Event;
use ratatui::Frame;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use verified_domain::id::{IdEntropy, SessionId, UnixMillis};

use crate::Limits;
use crate::client::{Answer, Client, Command, Position, StreamUpdate};
use crate::seam::{Clock, Dialer, Entropy, Input, Screen, TerminalError};

/// When and with what randomness an input event arrived: everything
/// `update` needs to mint ids without a clock or a random source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub at: UnixMillis,
    pub entropy: IdEntropy,
}

/// Which follower an update came from. The model picks a new one whenever it
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

/// An effect requested by `update`, done by the runtime.
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

/// The application contract.
pub trait Model: Sized {
    type Msg: From<Feedback> + Send + 'static;
    /// What the program starts from.
    type Flags;

    /// Initial state and startup effects.
    fn init(flags: Self::Flags) -> (Self, Cmd);

    /// Maps a terminal event to a message; `None` ignores it.
    fn on_event(&self, event: Event, stamp: Stamp) -> Option<Self::Msg>;

    /// Applies a message, returning follow-up effects. Pure.
    fn update(&mut self, msg: Self::Msg) -> Cmd;

    /// Draws the model. It may update render caches (never state `update`
    /// reads), hence `&mut self`.
    fn view(&mut self, frame: &mut Frame<'_>);
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
enum Incoming<Msg> {
    Event(Event),
    Msg(Msg),
    InputEnded,
    InputFailed(TerminalError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// The effects side of the loop.
struct World<Msg> {
    client: Client,
    inbox: mpsc::Sender<Incoming<Msg>>,
    tasks: JoinSet<()>,
    root: CancellationToken,
    follower: Option<CancellationToken>,
    limits: Limits,
}

impl<Msg: From<Feedback> + Send + 'static> World<Msg> {
    fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        assert!(
            self.tasks.len() < self.limits.tasks,
            "at most {} effect tasks run at once; the model asked for more",
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
                    let msg = Msg::from(Feedback::Answered(answer));
                    let _delivered = inbox.send(Incoming::Msg(msg)).await;
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
                        move |update| Incoming::Msg(Msg::from(Feedback::Stream { follow, update }));
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
async fn read_input<Msg: Send>(
    mut input: Box<dyn Input>,
    inbox: mpsc::Sender<Incoming<Msg>>,
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

/// Runs `M` until it quits or its input ends, and returns its final state.
///
/// `observe` sees the model after every redraw and after every update (several
/// updates may share one redraw); simulation uses it to check each folded
/// read and the transcript, and to script input in reaction to them.
///
/// # Errors
/// Drawing to or reading from the terminal failed.
///
/// # Panics
/// When an effect task panics, or the model starts more than
/// [`Limits::tasks`] effects at once.
pub async fn run<M: Model>(
    flags: M::Flags,
    limits: Limits,
    seams: Seams,
    screen: &mut dyn Screen,
    observe: &mut dyn FnMut(&M),
) -> Result<M, RunError> {
    let Seams {
        input,
        dialer,
        clock,
        mut entropy,
    } = seams;
    let (inbox, mut mailbox) = mpsc::channel::<Incoming<M::Msg>>(limits.inbox);
    let mut world = World {
        client: Client::new(dialer, Arc::clone(&clock), limits),
        inbox,
        tasks: JoinSet::new(),
        root: CancellationToken::new(),
        follower: None,
        limits,
    };
    let input_stop = world.root.child_token();
    let input_inbox = world.inbox.clone();
    world.spawn(read_input(input, input_inbox, input_stop));

    let (mut model, cmd) = M::init(flags);
    let mut flow = world.exec(cmd);
    let mut outcome = Ok(());
    while flow == Flow::Continue {
        if let Err(error) = screen.draw(&mut |frame| model.view(frame)) {
            outcome = Err(RunError::Terminal(error));
            break;
        }
        observe(&model);

        let first = tokio::select! {
            biased;
            incoming = mailbox.recv() => incoming,
            Some(joined) = world.tasks.join_next() => {
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
                    model.on_event(event, stamp)
                }
                Incoming::Msg(msg) => Some(msg),
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
                flow = world.exec(model.update(msg));
                observe(&model);
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
    world.root.cancel();
    mailbox.close();
    while let Some(joined) = world.tasks.join_next().await {
        reap(joined);
    }
    outcome.map(|()| model)
}
