//! End to end over localhost: the daemon's routes, the Durable Stream (catch
//! up and live SSE), and the real Anthropic client talking HTTP to the fake
//! Anthropic host. Binds tokio listeners directly, so it runs the same with
//! and without the `sim` feature.

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
use verified_domain::id::{MessageId, SessionId, UnixMillis};
use verified_server::daemon;
use verified_server::limits::ServerLimits;
use verified_server::log::{MemDisk, MemLogStore};
use verified_server::model::anthropic::{AnthropicConfig, AnthropicModel, ApiKey};
use verified_server::model::fake;
use verified_server::model::{Model as _, ModelError, ModelRequest};
use verified_server::seam::{Clock, OsEntropy, SystemClock, World};
use verified_server::shell::{Seams, ShellConfig};
use verified_wire::WireLimits;
use verified_wire::chunk::{TurnEndReason, UiChunk};
use verified_wire::command::{
    CreateSession, CreateSessionReply, MessageText, Outcome, Rejection, SendMessage,
    SendMessageReply, decode_reply,
};
use verified_wire::sse::SseDecoder;
use verified_wire::stream::{StreamEvent, decode_chunks, header};
use verified_wire::transcript::Transcript;

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

type HttpClient = Client<HttpConnector, Full<Bytes>>;

#[expect(
    clippy::disallowed_methods,
    reason = "the test binds real loopback listeners for the daemon and the fake host, with or without the sim shim"
)]
async fn listener() -> (tokio::net::TcpListener, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port");
    let addr = listener.local_addr().expect("bound");
    (listener, addr)
}

async fn start() -> (SocketAddr, World) {
    start_with(ServerLimits::v0()).await
}

async fn start_with(limits: ServerLimits) -> (SocketAddr, World) {
    let world = World::new();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let (fake_listener, fake_addr) = listener().await;
    let fake_app = fake::router(Arc::clone(&clock), world.clone());
    world.spawn(async move {
        drop(axum::serve(fake_listener, fake_app).await);
    });
    let model = AnthropicModel::new(
        HttpConnector::new(),
        AnthropicConfig {
            base: format!("http://{fake_addr}").parse().expect("a uri"),
            api_key: ApiKey::new("test-key".to_owned()),
            model: "claude-test".to_owned(),
        },
        &limits,
        Arc::clone(&clock),
    )
    .expect("a client");

    let (daemon_listener, daemon_addr) = listener().await;
    let seams = Seams {
        model: Arc::new(model),
        env: Arc::new(verified_server::env::LocalEnv::new(
            std::env::temp_dir(),
            Arc::clone(&clock),
            world.groups().clone(),
        )),
        clock,
        entropy: Arc::new(OsEntropy),
        world: world.clone(),
    };
    let config = ShellConfig {
        limits: limits.clone(),
        model: "claude-test".to_owned(),
        system: "test".to_owned(),
        planted: verified_server::shell::Planted::default(),
    };
    let store = Arc::new(MemLogStore::new(
        MemDisk::new(),
        limits.log_line_bytes,
        limits.log_file_bytes,
    ));
    world.spawn(async move {
        drop(daemon::serve(daemon_listener, store, seams, config).await);
    });
    (daemon_addr, world)
}

async fn call(
    client: &HttpClient,
    method: Method,
    uri: String,
    body: Vec<u8>,
    if_none_match: Option<&str>,
) -> (StatusCode, hyper::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(etag) = if_none_match {
        request = request.header("if-none-match", etag);
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .expect("a request");
    let response = client.request(request).await.expect("a response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn commands_and_streams_over_http() {
    let (addr, world) = start().await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let limits = WireLimits::DEFAULT;

    let (status, _, body) = call(
        &client,
        Method::GET,
        format!("{base}/version"),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "version");
    assert!(
        String::from_utf8_lossy(&body).contains(env!("CARGO_PKG_VERSION")),
        "the daemon's build"
    );

    let session = SessionId::from_parts(UnixMillis::new(1_700_000_000_000).expect("t"), &[3; 10]);
    let create = CreateSession {
        session_id: session,
    }
    .encode();
    let (status, _, body) = call(
        &client,
        Method::POST,
        format!("{base}/sessions"),
        create.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "created");
    let reply: CreateSessionReply = decode_reply(&body, &limits).expect("a reply");
    assert_eq!(reply.outcome, Outcome::Created, "new");
    let (status, _, _) = call(
        &client,
        Method::POST,
        format!("{base}/sessions"),
        create,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a repeat create is exists");

    let (status, _, _) = call(
        &client,
        Method::POST,
        format!("{base}/sessions/{session}/messages"),
        b"{\"nope\":1}".to_vec(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "malformed body");

    // Follow live before sending, like the TUI.
    let live = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "{base}/sessions/{session}/stream?offset=-1&live=sse"
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
    let (status, _, body) = call(
        &client,
        Method::POST,
        format!("{base}/sessions/{session}/messages"),
        message.encode(),
        None,
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

    // Catch-up read: the same chunks, cacheable by range.
    let stream = format!("{base}/sessions/{session}/stream?offset=-1");
    let (status, headers, body) =
        call(&client, Method::GET, stream.clone(), Vec::new(), None).await;
    assert_eq!(status, StatusCode::OK, "catch-up");
    let chunks = decode_chunks(&body).expect("chunks");
    let mut caught_up = Transcript::new(&limits);
    for chunk in &chunks {
        caught_up.apply(chunk).expect("folds");
    }
    assert_eq!(
        serde_json::to_value(caught_up.turns()).expect("json"),
        serde_json::to_value(transcript.turns()).expect("json"),
        "live and catch-up fold to the same transcript"
    );
    assert_eq!(
        headers
            .get(header::STREAM_UP_TO_DATE)
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "up to date"
    );
    let etag = headers
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("an etag")
        .to_owned();
    let (status, _, _) = call(&client, Method::GET, stream, Vec::new(), Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "same range, same etag");

    let (status, headers, _) = call(
        &client,
        Method::HEAD,
        format!("{base}/sessions/{session}/stream"),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "metadata");
    assert!(
        headers.get(header::STREAM_NEXT_OFFSET).is_some(),
        "the next offset"
    );

    let unknown = SessionId::from_parts(UnixMillis::new(1_700_000_000_000).expect("t"), &[9; 10]);
    let (status, _, _) = call(
        &client,
        Method::GET,
        format!("{base}/sessions/{unknown}/stream"),
        Vec::new(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown session");

    world.shutdown().cancel();
}

fn session_at(byte: u8) -> SessionId {
    SessionId::from_parts(UnixMillis::new(1_700_000_000_000).expect("t"), &[byte; 10])
}

fn rejected(status: StatusCode, body: &[u8]) -> (StatusCode, Rejection) {
    let rejection = decode_reply(body, &WireLimits::DEFAULT).expect("a typed rejection");
    (status, rejection)
}

async fn follow(
    client: &HttpClient,
    base: &str,
    session: SessionId,
) -> hyper::Response<hyper::body::Incoming> {
    let live = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "{base}/sessions/{session}/stream?offset=-1&live=sse"
        ))
        .body(Full::new(Bytes::new()))
        .expect("a request");
    client.request(live).await.expect("a response")
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
    let (addr, world) = start_with(limits).await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let wire = WireLimits::DEFAULT;
    for byte in [1u8, 2] {
        let create = CreateSession {
            session_id: session_at(byte),
        };
        let (status, _, _) = call(
            &client,
            Method::POST,
            format!("{base}/sessions"),
            create.encode(),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "session {byte}");
    }
    let third = CreateSession {
        session_id: session_at(3),
    };
    let (status, _, body) = call(
        &client,
        Method::POST,
        format!("{base}/sessions"),
        third.encode(),
        None,
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
    let (status, _, _) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(1, "hi").encode(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "under the bound when it started"
    );
    let stream = format!("{base}/sessions/{}/stream?offset=-1", session_at(1));
    let mut ended = false;
    for _ in 0..500u32 {
        let (_, _, body) = call(&client, Method::GET, stream.clone(), Vec::new(), None).await;
        let chunks = decode_chunks(&body).expect("chunks");
        if chunks
            .iter()
            .any(|chunk| matches!(chunk, UiChunk::TurnEnded { .. }))
        {
            ended = true;
            break;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    assert!(ended, "the first turn ends, past the bound");
    let (status, _, body) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(2, "more").encode(),
        None,
    )
    .await;
    let (status, rejection) = rejected(status, &body);
    assert_eq!(status, StatusCode::CONFLICT, "{rejection:?}");
    assert!(
        matches!(rejection, Rejection::SessionFull { session_id, .. } if session_id == session_at(1)),
        "{rejection:?}"
    );
    let (status, _, _) = call(
        &client,
        Method::POST,
        messages,
        message(1, "hi").encode(),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a repeat of an accepted message is still exists"
    );
    world.shutdown().cancel();
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
    World::new().spawn(async move {
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
    let model = AnthropicModel::new(
        HttpConnector::new(),
        AnthropicConfig {
            base: format!("http://{addr}").parse().expect("a uri"),
            api_key: ApiKey::new("test-key".to_owned()),
            model: "claude-test".to_owned(),
        },
        &limits,
        Arc::new(SystemClock),
    )
    .expect("a client");
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
