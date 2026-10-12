//! The daemon on turmoil's simulated network and filesystem (the `sim`
//! shim): a client drives a turn over HTTP, the daemon host crashes (losing
//! everything not synced) and restarts, and the stream still holds every
//! acknowledged record. Then a crash in the middle of a group commit.
#![cfg(feature = "sim")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use turmoil::barriers::{Barrier as SimBarrier, Reaction};
use verified_domain::id::{MessageId, SessionId, UnixMillis};
use verified_server::daemon::{self, Layout};
use verified_server::env::LocalEnv;
use verified_server::limits::ServerLimits;
use verified_server::model::fake::FakeModel;
use verified_server::model::{Model, ModelError, ModelRequest};
use verified_server::payload::ModelPart;
use verified_server::seam::{Clock, NoLock, OsEntropy, World};
use verified_server::shell::{Seams, ShellConfig};
use verified_server::shim::time::SimClock;
use verified_server::shim::{Barrier, fs, net};
use verified_wire::WireLimits;
use verified_wire::chunk::UiChunk;
use verified_wire::command::{CreateSession, MessageText, SendMessage};
use verified_wire::stream::decode_chunks;

use std::time::Duration;

/// `work`, or an error once `limit` passes: a test's own bound on how long
/// it waits for the code under test, never a seam of that code.
#[expect(
    clippy::disallowed_methods,
    reason = "a test's own deadline on waiting, not a timeout inside the code under test"
)]
async fn within<F: Future>(
    limit: Duration,
    work: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(limit, work).await
}

const PORT: u16 = 9000;

fn epoch() -> UnixMillis {
    UnixMillis::new(1_700_000_000_000).expect("a time")
}

fn session() -> SessionId {
    SessionId::from_parts(epoch(), &[5; 10])
}

fn daemon_host() -> impl Fn() -> std::pin::Pin<Box<dyn Future<Output = turmoil::Result> + 'static>>
{
    daemon_host_with(|clock| Arc::new(FakeModel::new(clock, 1000)))
}

fn daemon_host_with(
    model: fn(Arc<dyn Clock>) -> Arc<dyn Model>,
) -> impl Fn() -> std::pin::Pin<Box<dyn Future<Output = turmoil::Result> + 'static>> {
    move || {
        Box::pin(async move {
            fs::create_dir_all("/project".as_ref()).await?;
            fs::sync_dir("/".as_ref()).await?;
            let world = World::new();
            let clock: Arc<dyn Clock> = Arc::new(SimClock::new(epoch()));
            let seams = Seams {
                model: model(Arc::clone(&clock)),
                env: Arc::new(LocalEnv::new(
                    "/project".into(),
                    Arc::clone(&clock),
                    world.groups().clone(),
                )),
                clock,
                entropy: Arc::new(OsEntropy),
                world,
            };
            let config = ShellConfig {
                limits: ServerLimits::v0(),
                model: "fake".to_owned(),
                system: String::new(),
                planted: verified_server::shell::Planted::default(),
            };
            let listener =
                net::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT)).await?;
            let layout = Layout {
                project: "/project".into(),
            };
            daemon::run(listener, &layout, &NoLock, seams, config).await?;
            Ok(())
        })
    }
}

type HttpClient = Client<net::Connector, Full<Bytes>>;

fn client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build(net::Connector)
}

async fn post(client: &HttpClient, path: &str, body: Vec<u8>) -> turmoil::Result<u16> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("http://daemon:{PORT}{path}"))
        .body(Full::new(Bytes::from(body)))?;
    let response = client.request(request).await?;
    Ok(response.status().as_u16())
}

async fn chunks(client: &HttpClient) -> turmoil::Result<Vec<UiChunk>> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "http://daemon:{PORT}/sessions/{}/stream?offset=-1",
            session()
        ))
        .body(Full::new(Bytes::new()))?;
    let response = client.request(request).await?;
    let body = response.into_body().collect().await?.to_bytes();
    Ok(decode_chunks(&body, &WireLimits::DEFAULT)?)
}

fn turn_ended(chunks: &[UiChunk]) -> bool {
    chunks
        .iter()
        .any(|chunk| matches!(chunk, UiChunk::TurnEnded { .. }))
}

async fn send_hello(client: &HttpClient) -> turmoil::Result {
    post(
        client,
        "/sessions",
        CreateSession {
            session_id: session(),
        }
        .encode(),
    )
    .await?;
    let message = SendMessage {
        message_id: MessageId::from_parts(epoch(), &[6; 10]),
        text: MessageText::parse("hello sim".to_owned(), &WireLimits::DEFAULT)?,
    };
    post(
        client,
        &format!("/sessions/{}/messages", session()),
        message.encode(),
    )
    .await?;
    Ok(())
}

#[test]
fn acknowledged_records_survive_a_crash() -> turmoil::Result {
    let mut sim = turmoil::Builder::new().build();
    sim.host("daemon", daemon_host());
    sim.client("tui", async {
        let client = client();
        send_hello(&client).await?;
        loop {
            let read = chunks(&client).await?;
            if turn_ended(&read) {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    });
    sim.run()?;
    sim.crash("daemon");
    sim.bounce("daemon");
    sim.client("tui-after", async {
        let read = chunks(&client()).await?;
        assert!(turn_ended(&read), "the finished turn survived the crash");
        let echoed: String = read
            .iter()
            .filter_map(|chunk| {
                if let UiChunk::TextDelta { delta, .. } = chunk {
                    Some(delta.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(echoed, "Echo: hello sim", "every delta was durable");
        Ok(())
    });
    sim.run()
}

#[test]
fn crash_mid_group_commit_recovers() -> turmoil::Result {
    let mut sim = turmoil::Builder::new().build();
    sim.host("daemon", daemon_host());
    // Suspend the daemon inside the first group commit after the session
    // exists, then crash it there: written, not synced.
    let mut writing = SimBarrier::build(
        Reaction::Suspend,
        |point: &Barrier| matches!(point, Barrier::GroupWritten { bytes } if *bytes > 200),
    );
    sim.client("tui", async {
        let client = client();
        // The message's commit never acknowledges: the client gives up.
        drop(within(Duration::from_secs(5), send_hello(&client)).await);
        Ok(())
    });
    let mut suspended = None;
    while suspended.is_none() {
        sim.step()?;
        suspended = poll_once(writing.wait());
    }
    sim.crash("daemon");
    drop(suspended);
    drop(writing);
    sim.bounce("daemon");
    sim.client("tui-after", async {
        let client = client();
        // The message's commit was never acknowledged, so it is gone (cut as
        // a torn tail or never synced); retrying the same message id starts
        // the turn exactly once.
        let before = chunks(&client).await?;
        assert!(
            !before
                .iter()
                .any(|chunk| matches!(chunk, UiChunk::UserMessage { .. })),
            "an unacknowledged commit does not survive"
        );
        send_hello(&client).await?;
        loop {
            let read = chunks(&client).await?;
            if turn_ended(&read) {
                let messages = read
                    .iter()
                    .filter(|chunk| matches!(chunk, UiChunk::UserMessage { .. }))
                    .count();
                assert_eq!(messages, 1, "the retried message started one turn");
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    });
    sim.run()
}

/// A model whose stream panics: a broken invariant inside a world task.
#[derive(Debug)]
struct PanickingModel;

#[async_trait::async_trait]
impl Model for PanickingModel {
    #[expect(
        clippy::panic,
        reason = "the test's point: a broken invariant inside a world task"
    )]
    async fn stream(
        &self,
        _request: &ModelRequest,
        _parts: tokio::sync::mpsc::Sender<ModelPart>,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), ModelError> {
        panic!("the model task panics on purpose");
    }
}

/// Crash-only under the simulation: a panic in a world task cannot abort
/// the test process (every seed shares it), so it fails the daemon's host
/// instead. The run ends with that error rather than hanging with a
/// half-dead daemon still serving.
#[test]
fn a_panicking_task_fails_the_daemon_host() {
    let mut sim = turmoil::Builder::new().build();
    sim.host("daemon", daemon_host_with(|_| Arc::new(PanickingModel)));
    sim.client("tui", async {
        let client = client();
        send_hello(&client).await?;
        // The daemon is gone: nothing ever answers again.
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

/// Polls `future` once without a runtime: barriers resolve synchronously
/// once triggered.
fn poll_once<F: Future>(future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(output) => Some(output),
        std::task::Poll::Pending => None,
    }
}
