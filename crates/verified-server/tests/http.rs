//! End to end over localhost: the daemon's routes, the Durable Stream (live
//! SSE), and the real Anthropic client talking HTTP to the fake
//! Anthropic host. Binds tokio listeners directly, so it runs the same with
//! and without the `sim` feature.
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test harness code, not the code under test"
)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use verified_domain::Millis;
use verified_domain::id::{ActorId, MessageId, UnixMillis};
use verified_server::daemon;
use verified_server::limits::ServerLimits;
use verified_server::log::{MemDisk, MemLogStore};
use verified_server::model::{Model as _, ModelError, ModelRequest};
use verified_server::seam::{Clock, OsEntropy, SystemClock, Tasks};
use verified_server::shell::{Seams, ShellConfig};
use verified_wire::WireLimits;
use verified_wire::chunk::{TurnEndReason, UiChunk};
use verified_wire::command::{
    CreateSession, CreateSessionReply, MessageText, Outcome, Rejection, SendMessage,
    SendMessageReply, decode_reply,
};
use verified_wire::sse::SseDecoder;
use verified_wire::stream::{Freshness, StreamEvent};
use verified_wire::transcript::Transcript;

use common::{client, fake_model, listener, within};

type HttpClient = Client<HttpConnector, Full<Bytes>>;

async fn start() -> (SocketAddr, Tasks) {
    start_with(ServerLimits::v0()).await
}

async fn start_with(limits: ServerLimits) -> (SocketAddr, Tasks) {
    let tasks = Tasks::new();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let model = fake_model(&limits, &clock, &tasks).await;

    let (daemon_listener, daemon_addr) = listener().await;
    let seams = Seams {
        model: Arc::new(model),
        env: Arc::new(verified_server::env::LocalEnv::new(
            std::env::temp_dir(),
            Arc::clone(&clock),
            tasks.groups().clone(),
        )),
        clock,
        entropy: Arc::new(OsEntropy),
        tasks: tasks.clone(),
    };
    let config = ShellConfig {
        limits: limits.clone(),
        model: "claude-test".to_owned(),
        system: "test".to_owned(),
        planted: verified_server::shell::Planted::None,
    };
    let store = Arc::new(MemLogStore::new(
        MemDisk::new(),
        limits.log_line_bytes,
        limits.log_file_bytes,
    ));
    tasks.spawn(async move {
        drop(daemon::serve(daemon_listener, store, seams, config).await);
    });
    (daemon_addr, tasks)
}

async fn call(
    client: &HttpClient,
    method: Method,
    uri: String,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Full::new(Bytes::from(body)))
        .expect("a request");
    let response = client.request(request).await.expect("a response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn commands_and_streams_over_http() {
    let (addr, tasks) = start().await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let limits = WireLimits::DEFAULT;

    let (status, body) = call(&client, Method::GET, format!("{base}/version"), Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "version");
    assert!(
        String::from_utf8_lossy(&body).contains(env!("CARGO_PKG_VERSION")),
        "the daemon's build"
    );

    let session = ActorId::from_parts(UnixMillis::new(1_700_000_000_000).expect("t"), &[3; 10]);
    let create = CreateSession {
        session_id: session,
    }
    .encode();
    let (status, body) = call(
        &client,
        Method::POST,
        format!("{base}/sessions"),
        create.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "created");
    let reply: CreateSessionReply = decode_reply(&body, &limits).expect("a reply");
    assert_eq!(reply.outcome, Outcome::Created, "new");
    let (status, _) = call(&client, Method::POST, format!("{base}/sessions"), create).await;
    assert_eq!(status, StatusCode::OK, "a repeat create is exists");

    let (status, _) = call(
        &client,
        Method::POST,
        format!("{base}/sessions/{session}/messages"),
        b"{\"nope\":1}".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "malformed body");

    // Follow live before sending, like the TUI.
    let live = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "{base}/sessions/{session}/stream?offset=0000000000000000"
        ))
        .body(Full::new(Bytes::new()))
        .expect("a request");
    let response = client.request(live).await.expect("live");
    assert_eq!(response.status(), StatusCode::OK, "live stream");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream"),
        "SSE"
    );
    let mut live_body = response.into_body();

    let message = SendMessage {
        message_id: MessageId::from_parts(UnixMillis::new(1_700_000_000_001).expect("t"), &[4; 10]),
        text: MessageText::parse("hello over http".to_owned(), &limits).expect("text"),
    };
    let (status, body) = call(
        &client,
        Method::POST,
        format!("{base}/sessions/{session}/messages"),
        message.encode(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "accepted");
    let sent: SendMessageReply = decode_reply(&body, &limits).expect("a reply");

    // Fold the live stream until the turn ends.
    let mut decoder = SseDecoder::new(&limits);
    let mut transcript = Transcript::new(&limits);
    let mut controls = 0u32;
    let mut done = false;
    while !done {
        let frame = within(Duration::from_secs(20), live_body.frame())
            .await
            .expect("the stream moves")
            .expect("not ended")
            .expect("a frame");
        let Ok(bytes) = frame.into_data() else {
            continue;
        };
        for event in decoder.feed(&bytes) {
            match StreamEvent::from_sse(&event.expect("an event")).expect("a stream event") {
                StreamEvent::Data(chunks) => {
                    for chunk in &chunks {
                        transcript.apply(chunk).expect("folds");
                        if matches!(chunk, UiChunk::TurnEnded { id, .. } if *id == sent.turn_id) {
                            done = true;
                        }
                    }
                }
                StreamEvent::Control(_) => controls = controls.saturating_add(1),
            }
        }
    }
    assert!(controls >= 1, "every read is followed by a control event");
    let turn = transcript.turns().first().expect("the turn");
    assert_eq!(turn.end(), Some(TurnEndReason::Completed), "completed");
    let text: String = turn
        .reply()
        .map(|reply| {
            reply
                .parts
                .iter()
                .filter_map(|part| {
                    if let verified_wire::transcript::Part::Text { text, .. } = part {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        text, "Echo: hello over http",
        "the real client parsed the fake's SSE"
    );

    let unknown = ActorId::from_parts(UnixMillis::new(1_700_000_000_000).expect("t"), &[9; 10]);
    let (status, _) = call(
        &client,
        Method::GET,
        format!("{base}/sessions/{unknown}/stream"),
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown session");

    tasks.shutdown().cancel();
}

fn session_at(byte: u8) -> ActorId {
    ActorId::from_parts(UnixMillis::new(1_700_000_000_000).expect("t"), &[byte; 10])
}

fn rejected(status: StatusCode, body: &[u8]) -> (StatusCode, Rejection) {
    let rejection = decode_reply(body, &WireLimits::DEFAULT).expect("a typed rejection");
    (status, rejection)
}

async fn follow(
    client: &HttpClient,
    base: &str,
    session: ActorId,
) -> hyper::Response<hyper::body::Incoming> {
    let live = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "{base}/sessions/{session}/stream?offset=0000000000000000"
        ))
        .body(Full::new(Bytes::new()))
        .expect("a request");
    client.request(live).await.expect("a response")
}

/// Every chunk of `session`'s stream, followed live until a control event
/// says it is up to date; `None` when the follow is refused.
async fn caught_up(client: &HttpClient, base: &str, session: ActorId) -> Option<Vec<UiChunk>> {
    let response = follow(client, base, session).await;
    if response.status() != StatusCode::OK {
        return None;
    }
    let mut body = response.into_body();
    let mut decoder = SseDecoder::new(&WireLimits::DEFAULT);
    let mut chunks = Vec::new();
    loop {
        let frame = body.frame().await.expect("not ended").expect("a frame");
        let Ok(bytes) = frame.into_data() else {
            continue;
        };
        for event in decoder.feed(&bytes) {
            match StreamEvent::from_sse(&event.expect("an event")).expect("a stream event") {
                StreamEvent::Data(read) => chunks.extend(read),
                StreamEvent::Control(control) => {
                    if control.up_to_date == Freshness::UpToDate {
                        return Some(chunks);
                    }
                }
            }
        }
    }
}

async fn body_of(response: hyper::Response<hyper::body::Incoming>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes()
        .to_vec()
}

/// Every capacity bound answers with a typed rejection: sessions per
/// daemon, live followers per session and per daemon (released when a
/// follower ends), and bytes per session (a repeat still answers exists).
#[tokio::test]
async fn capacity_limits_answer_typed_rejections() {
    let mut limits = ServerLimits::v0();
    limits.sessions = 2;
    limits.sse_followers_per_session = 1;
    limits.sse_followers = 1;
    limits.sse_keep_alive = Millis::new(20);
    // Room for a new session's first records (its epoch), not for a turn.
    limits.session_bytes = 512;
    let (addr, tasks) = start_with(limits).await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let wire = WireLimits::DEFAULT;
    for byte in [1u8, 2] {
        let create = CreateSession {
            session_id: session_at(byte),
        };
        let (status, _) = call(
            &client,
            Method::POST,
            format!("{base}/sessions"),
            create.encode(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "session {byte}");
    }
    let third = CreateSession {
        session_id: session_at(3),
    };
    let (status, body) = call(
        &client,
        Method::POST,
        format!("{base}/sessions"),
        third.encode(),
    )
    .await;
    assert_eq!(
        rejected(status, &body),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Rejection::TooManySessions { limit: 2 }
        ),
        "the daemon is full"
    );

    let first = follow(&client, &base, session_at(1)).await;
    assert_eq!(first.status(), StatusCode::OK, "one follower");
    let again = follow(&client, &base, session_at(1)).await;
    let status = again.status();
    assert_eq!(
        rejected(status, &body_of(again).await),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Rejection::TooManyFollowers {
                session_id: session_at(1),
                limit: 1
            }
        ),
        "the session's follower limit"
    );
    let other = follow(&client, &base, session_at(2)).await;
    let status = other.status();
    assert_eq!(
        rejected(status, &body_of(other).await),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Rejection::TooManyFollowers {
                session_id: session_at(2),
                limit: 1
            }
        ),
        "the daemon's follower limit"
    );
    drop(first);
    let mut freed = false;
    for _ in 0..200u32 {
        let retry = follow(&client, &base, session_at(2)).await;
        if retry.status() == StatusCode::OK {
            freed = true;
            break;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    assert!(freed, "a gone follower releases its slots at its next send");

    let message = |byte: u8, text: &str| SendMessage {
        message_id: MessageId::from_parts(
            UnixMillis::new(1_700_000_000_001).expect("t"),
            &[byte; 10],
        ),
        text: MessageText::parse(text.to_owned(), &wire).expect("text"),
    };
    let messages = format!("{base}/sessions/{}/messages", session_at(1));
    let (status, _) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(1, "hi").encode(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "under the bound when it started"
    );
    let mut ended = false;
    for _ in 0..500u32 {
        // Refused while the last follower still holds the daemon's slot.
        if let Some(chunks) = caught_up(&client, &base, session_at(1)).await
            && chunks
                .iter()
                .any(|chunk| matches!(chunk, UiChunk::TurnEnded { .. }))
        {
            ended = true;
            break;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    assert!(ended, "the first turn ends, past the bound");
    let (status, body) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(2, "more").encode(),
    )
    .await;
    let (status, rejection) = rejected(status, &body);
    assert_eq!(status, StatusCode::CONFLICT, "{rejection:?}");
    assert!(
        matches!(rejection, Rejection::SessionFull { session_id, .. } if session_id == session_at(1)),
        "{rejection:?}"
    );
    let (status, _) = call(&client, Method::POST, messages, message(1, "hi").encode()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a repeat of an accepted message is still exists"
    );
    tasks.shutdown().cancel();
}

const fn request() -> ModelRequest {
    ModelRequest {
        system: String::new(),
        messages: Vec::new(),
        max_tokens: 16,
    }
}

/// A model host that accepts and then answers `reply` (and nothing more).
async fn stalled_host(reply: &'static [u8]) -> SocketAddr {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (listener, addr) = listener().await;
    Tasks::new().spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 64 << 10];
            drop(stream.read(&mut buffer).await);
            drop(stream.write_all(reply).await);
            held.push(stream);
        }
    });
    addr
}

async fn stalled_call(reply: &'static [u8]) -> Result<(), ModelError> {
    let addr = stalled_host(reply).await;
    let mut limits = ServerLimits::v0();
    limits.model_response_timeout = Millis::new(100);
    limits.model_idle_timeout = Millis::new(100);
    let model = client(addr, &limits, Arc::new(SystemClock));
    let (parts, _receiver) = tokio::sync::mpsc::channel(16);
    within(
        Duration::from_secs(10),
        model.stream(
            &request(),
            parts,
            &tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await
    .expect("the model's own timeouts end the call")
}

/// A model host that goes quiet is a retryable network failure, before the
/// response head and between body frames.
#[tokio::test]
async fn a_silent_model_times_out_as_a_network_failure() {
    let silent = stalled_call(b"").await;
    assert!(
        matches!(&silent, Err(ModelError::Network(why)) if why.contains("response head")),
        "{silent:?}"
    );
    let idle = stalled_call(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
    )
    .await;
    assert!(
        matches!(&idle, Err(ModelError::Network(why)) if why.contains("a gap in the response")),
        "{idle:?}"
    );
}
