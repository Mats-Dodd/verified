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
    ActorList, ActorState, CreateActor, CreateActorReply, MessageText, Outcome, Rejection,
    SendMessage, SendMessageReply, StopActor, StopReply, decode_reply,
};
use verified_wire::sse::SseDecoder;
use verified_wire::stream::{Freshness, StreamEvent};
use verified_wire::transcript::{Part, ToolState, Transcript};

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
        env: Arc::new(verified_server::env::NoEnv),
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
    let create = CreateActor { actor_id: session }.encode();
    let (status, body) = call(
        &client,
        Method::POST,
        format!("{base}/actors"),
        create.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "created");
    let reply: CreateActorReply = decode_reply(&body, &limits).expect("a reply");
    assert_eq!(reply.outcome, Outcome::Created, "new");
    let (status, _) = call(&client, Method::POST, format!("{base}/actors"), create).await;
    assert_eq!(status, StatusCode::OK, "a repeat create is exists");

    let (status, _) = call(
        &client,
        Method::POST,
        format!("{base}/actors/{session}/messages"),
        b"{\"nope\":1}".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "malformed body");

    // Follow live before sending, like the TUI.
    let live = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "{base}/actors/{session}/stream?offset=0000000000000000"
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
        format!("{base}/actors/{session}/messages"),
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
                    if let Part::Text { text, .. } = part {
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
        format!("{base}/actors/{unknown}/stream"),
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
            "{base}/actors/{session}/stream?offset=0000000000000000"
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
    limits.actors = 2;
    limits.sse_followers_per_actor = 1;
    limits.sse_followers = 1;
    limits.sse_keep_alive = Millis::new(20);
    // Room for a new session's first records (its epoch), not for a turn.
    limits.actor_bytes = 512;
    let (addr, tasks) = start_with(limits).await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let wire = WireLimits::DEFAULT;
    for byte in [1u8, 2] {
        let create = CreateActor {
            actor_id: session_at(byte),
        };
        let (status, _) = call(
            &client,
            Method::POST,
            format!("{base}/actors"),
            create.encode(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "session {byte}");
    }
    let third = CreateActor {
        actor_id: session_at(3),
    };
    let (status, body) = call(
        &client,
        Method::POST,
        format!("{base}/actors"),
        third.encode(),
    )
    .await;
    assert_eq!(
        rejected(status, &body),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Rejection::TooManyActors { limit: 2 }
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
                actor_id: session_at(1),
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
                actor_id: session_at(2),
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
    let messages = format!("{base}/actors/{}/messages", session_at(1));
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
        matches!(rejection, Rejection::ActorFull { actor_id, .. } if actor_id == session_at(1)),
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

/// Folds `actor`'s stream from the start until `done` holds of the
/// transcript, polling.
async fn fold_until(
    client: &HttpClient,
    base: &str,
    actor: ActorId,
    done: impl Fn(&Transcript) -> bool,
) -> Transcript {
    let mut folded = None;
    for _ in 0..1000u32 {
        if let Some(chunks) = caught_up(client, base, actor).await {
            let mut transcript = Transcript::new(&WireLimits::DEFAULT);
            for chunk in &chunks {
                transcript.apply(chunk).expect("folds");
            }
            if done(&transcript) {
                folded = Some(transcript);
                break;
            }
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    folded.expect("the stream gets there")
}

async fn actors(client: &HttpClient, base: &str) -> ActorList {
    let (status, body) = call(client, Method::GET, format!("{base}/actors"), Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "the actor list");
    decode_reply(&body, &WireLimits::DEFAULT).expect("an actor list")
}

/// Actors are addressed by id. A message to a busy actor is answered once
/// it is durably in the mailbox, and steers the running turn: the call
/// waiting for the user is denied, and the next step carries the message.
/// `GET /actors` tags each actor's waiting approvals.
#[tokio::test]
async fn a_message_to_a_busy_actor_steers_its_running_turn() {
    let (addr, tasks) = start().await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let wire = WireLimits::DEFAULT;
    let (busy, idle) = (session_at(5), session_at(6));
    for actor in [busy, idle] {
        let create = CreateActor { actor_id: actor }.encode();
        let (status, _) = call(&client, Method::POST, format!("{base}/actors"), create).await;
        assert_eq!(status, StatusCode::CREATED, "{actor}");
    }
    let message = |byte: u8, text: &str| SendMessage {
        message_id: MessageId::from_parts(
            UnixMillis::new(1_700_000_000_001).expect("t"),
            &[byte; 10],
        ),
        text: MessageText::parse(text.to_owned(), &wire).expect("text"),
    };
    let messages = format!("{base}/actors/{busy}/messages");
    let (status, body) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(1, "edit lib.rs a b").encode(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "the first message");
    let first: SendMessageReply = decode_reply(&body, &wire).expect("a reply");
    // No rule allows `edit`: the turn waits for the user.
    let waiting = fold_until(&client, &base, busy, |transcript| {
        transcript.running().is_some_and(|turn| {
            turn.reply().is_some_and(|reply| {
                reply.parts.iter().any(|part| {
                    matches!(part, Part::Tool(tool) if tool.state == ToolState::ApprovalRequested)
                })
            })
        })
    })
    .await;
    let call_id = waiting
        .running()
        .and_then(verified_wire::transcript::TranscriptTurn::reply)
        .and_then(|reply| {
            reply.parts.iter().find_map(|part| match part {
                Part::Tool(tool) => Some(tool.tool_call_id.clone()),
                Part::StepStart | Part::Text { .. } => None,
            })
        })
        .expect("the edit");

    let listed = actors(&client, &base).await;
    let summary = |list: &ActorList, actor: ActorId| {
        list.actors
            .iter()
            .find(|summary| summary.actor_id == actor)
            .cloned()
            .expect("listed")
    };
    let asking = summary(&listed, busy);
    assert_eq!(asking.name, "main");
    assert_eq!(
        asking.state,
        ActorState::Running {
            turn_id: first.turn_id
        }
    );
    assert_eq!(
        asking.approvals,
        core::slice::from_ref(&call_id),
        "tagged by actor"
    );
    let quiet = summary(&listed, idle);
    assert_eq!(quiet.state, ActorState::Idle);
    assert!(quiet.approvals.is_empty(), "the other actor asks nothing");

    // Steer: answered at once, though the turn runs.
    let (status, body) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(2, "hello instead").encode(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a busy actor takes the message"
    );
    let steer: SendMessageReply = decode_reply(&body, &wire).expect("a reply");
    assert_eq!(steer.turn_id, first.turn_id, "it steers the running turn");
    let (status, body) = call(
        &client,
        Method::POST,
        messages.clone(),
        message(2, "hello instead").encode(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a repeat is exists");
    let repeat: SendMessageReply = decode_reply(&body, &wire).expect("a reply");
    assert_eq!(repeat.turn_id, first.turn_id, "the same turn");

    let done = fold_until(&client, &base, busy, |transcript| {
        transcript
            .turns()
            .first()
            .is_some_and(|turn| turn.end().is_some())
    })
    .await;
    assert_eq!(
        done.turns().len(),
        1,
        "the steer started no turn of its own"
    );
    let turn = done.turns().first().expect("the turn");
    assert_eq!(turn.end(), Some(TurnEndReason::Completed));
    assert_eq!(
        turn.inputs()
            .iter()
            .map(|input| (input.mail.text.as_str(), input.step))
            .collect::<Vec<_>>(),
        [("edit lib.rs a b", 0), ("hello instead", 1)],
        "the next step carried the steer"
    );
    let parts = turn
        .reply()
        .map(|reply| reply.parts.to_vec())
        .unwrap_or_default();
    assert!(
        parts.iter().any(|part| matches!(
            part,
            Part::Tool(tool) if tool.tool_call_id == call_id && tool.state == ToolState::OutputDenied
        )),
        "the waiting call was denied: {parts:?}"
    );
    assert!(
        parts.iter().any(|part| matches!(
            part,
            Part::Text { text, .. } if text == "Echo: hello instead"
        )),
        "the model saw the steer: {parts:?}"
    );
    assert!(done.queued().is_empty(), "nothing left unread");
    let after = summary(&actors(&client, &base).await, busy);
    assert_eq!(after.state, ActorState::Idle);
    assert_eq!(after.approvals, []);
    assert_eq!(after.unread, 0);

    tasks.shutdown().cancel();
}

/// Your stop over HTTP: durable before it is answered, a repeat is
/// `exists`, and a stopped actor takes no other stop and no message.
#[tokio::test]
async fn a_stop_over_http_stops_the_actor() {
    let (addr, tasks) = start().await;
    let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let base = format!("http://{addr}");
    let wire = WireLimits::DEFAULT;
    let actor = session_at(8);
    let create = CreateActor { actor_id: actor }.encode();
    let (status, _) = call(&client, Method::POST, format!("{base}/actors"), create).await;
    assert_eq!(status, StatusCode::CREATED);
    let at = UnixMillis::new(1_700_000_000_001).expect("t");
    let stop = |byte: u8| {
        StopActor {
            stop_id: MessageId::from_parts(at, &[byte; 10]),
        }
        .encode()
    };
    let route = format!("{base}/actors/{actor}/stop");
    let (status, body) = call(&client, Method::POST, route.clone(), stop(1)).await;
    assert_eq!(status, StatusCode::CREATED, "the stop");
    let reply: StopReply = decode_reply(&body, &wire).expect("a reply");
    assert_eq!(reply.outcome, Outcome::Created);
    let (status, body) = call(&client, Method::POST, route.clone(), stop(1)).await;
    assert_eq!(status, StatusCode::OK, "a repeat");
    let reply: StopReply = decode_reply(&body, &wire).expect("a reply");
    assert_eq!(reply.outcome, Outcome::Exists);
    let (status, body) = call(&client, Method::POST, route, stop(2)).await;
    assert_eq!(
        rejected(status, &body),
        (
            StatusCode::CONFLICT,
            Rejection::ActorStopped { actor_id: actor }
        ),
        "another stop"
    );
    let message = SendMessage {
        message_id: MessageId::from_parts(at, &[3; 10]),
        text: MessageText::parse("hello".to_owned(), &wire).expect("text"),
    };
    let (status, body) = call(
        &client,
        Method::POST,
        format!("{base}/actors/{actor}/messages"),
        message.encode(),
    )
    .await;
    assert_eq!(
        rejected(status, &body),
        (
            StatusCode::CONFLICT,
            Rejection::ActorStopped { actor_id: actor }
        ),
        "a message"
    );
    let listed = actors(&client, &base).await;
    assert!(
        listed
            .actors
            .iter()
            .any(|summary| summary.actor_id == actor && summary.state == ActorState::Stopped),
        "{listed:?}"
    );
    tasks.shutdown().cancel();
}

const fn request() -> ModelRequest {
    ModelRequest {
        system: String::new(),
        messages: Vec::new(),
        max_tokens: 16,
        tools: Vec::new(),
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
