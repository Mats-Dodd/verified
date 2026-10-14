//! The daemon on turmoil, driven by the TUI's client: a panic in a
//! daemon task fails its host (crash-only under the simulation), and a
//! session holding over 4 MiB of tool output is followed from the start.
#![expect(clippy::panic, reason = "test harness code, not the code under test")]

use core::time::Duration;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::id::{ActorId, MessageId};
use verified_domain::{Limits as CoreLimits, LimitsConfig};
use verified_server::daemon::{self, Layout};
use verified_server::env::LocalEnv;
use verified_server::limits::ServerLimits;
use verified_server::model::{Model, ModelError, ModelRequest};
use verified_server::payload::ModelPart;
use verified_server::seam::{Clock, NoLock, Tasks};
use verified_server::shell::{Planted, Seams, ShellConfig};
use verified_server::shim::{fs, net};
use verified_sim::clock::SimClock;
use verified_sim::entropy::SeededEntropy;
use verified_sim::hosts::{ANTHROPIC, DAEMON, PORT, Software, anthropic, epoch, model};
use verified_tui::Limits;
use verified_tui::client::{Answer, Client, ClientError, Command, StreamUpdate};
use verified_tui::seam::TcpDialer;
use verified_wire::WireLimits;
use verified_wire::chunk::UiChunk;
use verified_wire::command::{CreateActor, MessageText, SendMessage};
use verified_wire::stream::Offset;

/// The daemon under `limits` with `model`, its project holding `files` (a
/// name and a size each, filled with `x`).
fn daemon_host(
    model: fn(&ServerLimits, Arc<dyn Clock>) -> Arc<dyn Model>,
    limits: ServerLimits,
    files: &'static [(&'static str, usize)],
) -> impl Fn() -> Software {
    move || {
        let limits = limits.clone();
        Box::pin(async move {
            let project = Path::new("/project");
            fs::create_dir_all(project).await?;
            for (name, size) in files {
                fs::write_synced(&project.join(name), &vec![b'x'; *size]).await?;
            }
            fs::sync_dir(Path::new("/")).await?;
            let tasks = Tasks::new();
            let clock: Arc<dyn Clock> = Arc::new(SimClock::new(epoch()));
            let seams = Seams {
                model: model(&limits, Arc::clone(&clock)),
                env: Arc::new(LocalEnv::new(
                    PathBuf::from(project),
                    Arc::clone(&clock),
                    tasks.groups().clone(),
                )),
                clock,
                entropy: Arc::new(SeededEntropy::new(0)),
                tasks,
            };
            let config = ShellConfig {
                limits,
                model: "fake".to_owned(),
                system: String::new(),
                planted: Planted::None,
            };
            let listener =
                net::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT)).await?;
            let layout = Layout {
                project: PathBuf::from(project),
            };
            daemon::run(listener, &layout, &NoLock, seams, config).await?;
            Ok(())
        })
    }
}

fn client() -> Client {
    Client::new(
        Arc::new(TcpDialer::new(DAEMON.to_owned(), PORT)),
        Arc::new(SimClock::new(epoch())),
        Limits::DEFAULT,
    )
}

fn session() -> ActorId {
    ActorId::from_parts(epoch(), &[5; 10])
}

fn create() -> Command {
    Command::CreateActor(CreateActor {
        actor_id: session(),
    })
}

fn message(id: u8, text: &str) -> Command {
    Command::SendMessage {
        actor: session(),
        body: SendMessage {
            message_id: MessageId::from_parts(epoch(), &[id; 10]),
            text: MessageText::parse(text.to_owned(), &WireLimits::DEFAULT)
                .expect("the text fits the wire"),
        },
    }
}

/// Sends `command`: whether the daemon took it.
async fn send(client: &Client, command: Command) -> Result<(), ClientError> {
    match client.send(command).await {
        Answer::ActorCreated { result, .. } => result.map(drop),
        Answer::Actors(result) => result.map(drop),
        Answer::MessageSent { result, .. } => result.map(drop),
        Answer::Approved { result, .. } => result.map(drop),
        Answer::Cancelled { result, .. } => result.map(drop),
    }
}

fn count(chunks: &[UiChunk], kind: impl Fn(&UiChunk) -> bool) -> usize {
    chunks.iter().filter(|chunk| kind(chunk)).count()
}

/// Follows the actor's stream from the start until `done` holds of what
/// arrived.
async fn followed(
    client: &Client,
    done: impl Fn(&[UiChunk]) -> bool,
) -> Result<Vec<UiChunk>, ClientError> {
    let stop = CancellationToken::new();
    let (inbox, mut updates) = mpsc::channel(8);
    let follower = async {
        client
            .follow(session(), Offset::BEGINNING, &inbox, |update| update, &stop)
            .await;
        drop(inbox);
    };
    let collector = async {
        let mut seen = Vec::new();
        while let Some(update) = updates.recv().await {
            match update {
                StreamUpdate::Read { chunks, .. } => {
                    seen.extend(chunks);
                    if done(&seen) {
                        stop.cancel();
                    }
                }
                StreamUpdate::Connected => {}
                StreamUpdate::Disconnected { error, .. } | StreamUpdate::Ended { error } => {
                    return Err(error);
                }
            }
        }
        Ok(seen)
    };
    let ((), seen) = tokio::join!(follower, collector);
    seen
}

/// A model whose stream panics: a broken invariant inside a daemon task.
#[derive(Debug)]
struct PanickingModel;

#[async_trait::async_trait]
impl Model for PanickingModel {
    async fn stream(
        &self,
        _request: &ModelRequest,
        _parts: mpsc::Sender<ModelPart>,
        _cancel: &CancellationToken,
    ) -> Result<(), ModelError> {
        panic!("the model task panics on purpose");
    }
}

/// Crash-only under the simulation: a panic in a daemon task cannot abort
/// the test process (every seed shares it), so it fails the daemon's host
/// instead. The run ends with that error rather than hanging with a
/// half-dead daemon still serving.
#[test]
fn a_panicking_task_fails_the_daemon_host() {
    let mut sim = turmoil::Builder::new().build();
    sim.host(
        DAEMON,
        daemon_host(|_, _| Arc::new(PanickingModel), ServerLimits::v0(), &[]),
    );
    sim.client("tui", async {
        let client = client();
        send(&client, create()).await?;
        // The daemon is gone once the model task panics: nothing answers.
        drop(send(&client, message(6, "hello sim")).await);
        std::future::pending::<()>().await;
        Ok(())
    });
    // Without the crash the run would end at the simulation's time limit,
    // with a different error.
    let error = sim
        .run()
        .expect_err("the run cannot finish while the client waits forever");
    assert!(
        error.to_string().contains("a daemon task panicked"),
        "the daemon host failed with the crash: {error}"
    );
}

/// Over 4 MiB of tool output (84 reads of 64 KiB) is followed from the
/// start: no read is a data event too large for the client to decode.
#[test]
fn a_large_session_is_followed_from_the_start() -> turmoil::Result {
    let mut limits = ServerLimits::v0();
    limits.core = CoreLimits::new(LimitsConfig {
        context_bytes: core::num::NonZeroU64::new(64u64 << 20u32).expect("non-zero"),
        ..LimitsConfig::v0()
    });
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(600))
        .build();
    sim.host(ANTHROPIC, anthropic());
    sim.host(
        DAEMON,
        daemon_host(
            |limits, clock| Arc::new(model(limits, clock).expect("a client")),
            limits,
            &[("big.txt", 64usize << 10u32)],
        ),
    );
    sim.client("tui", async {
        let client = client();
        send(&client, create()).await?;
        let reads = vec!["read big.txt"; 14].join(" ; ");
        // A message to a busy actor steers its turn, so the six are all
        // accepted at once; the work is done once every read's output is
        // in and no turn runs. Only then is the stream read from the
        // start, so its first read has over 4 MiB to serve.
        for turn in 1..=6u8 {
            send(&client, message(turn, &reads)).await?;
        }
        let quiet = |seen: &[UiChunk]| {
            count(seen, |chunk| {
                matches!(chunk, UiChunk::ToolOutputAvailable { .. })
            }) == 84
                && count(seen, |chunk| matches!(chunk, UiChunk::TurnStarted { .. }))
                    == count(seen, |chunk| matches!(chunk, UiChunk::TurnEnded { .. }))
        };
        followed(&client, quiet).await?;
        let seen = followed(&client, quiet).await;
        let outputs = count(&seen?, |chunk| {
            matches!(chunk, UiChunk::ToolOutputAvailable { .. })
        });
        assert_eq!(outputs, 84, "every read's output is followed");
        Ok(())
    });
    sim.run()
}
