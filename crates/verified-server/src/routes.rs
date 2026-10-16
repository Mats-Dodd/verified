//! HTTP routes: commands in, the Durable Stream out. Every actor is
//! addressed by its id.
//!
//! - `POST /actors`, `/actors/{id}/messages`, `/approvals`, `/cancel`,
//!   `/stop`: commands with client-minted ids; a repeat answers `exists`
//!   (`TigerBeetle`), a conflicting repeat is a 409 rejection. A message is
//!   answered once it is durably in the actor's mailbox; to a busy actor it
//!   steers the running turn.
//! - `POST /actors/{id}/compact`: compacts the actor's history between
//!   turns. It carries no id: a repeat compacts again.
//! - `GET /actors`: every born actor, with who waits for the user and how
//!   many tasks run, from each actor's live session.
//! - `GET /actors/{id}/stream?offset=…`: SSE from the offset, then every
//!   append, each read followed by a `control` event (Durable Streams §5.8);
//!   closed after `ServerLimits::sse_lifetime` so clients reconnect from the
//!   last offset. This is the part of Durable Streams the TUI uses; a web
//!   client gets the rest of the protocol when it arrives.
//! - `GET /version`: the daemon's build, so a client of another build knows.
//!
//! Ported from q's `http/SessionStreamHttp.ts`.

use core::fmt;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use verified_domain::id::ActorId;
use verified_wire::command::{
    ActorList, Approve, Cancel, Compact, CreateActor, CreateActorReply, Outcome as WireOutcome,
    Rejection, SendMessage, StopActor, encode_reply,
};
use verified_wire::sse::KEEP_ALIVE;
use verified_wire::stream::{
    Control, Freshness, Offset, StreamEvent, header as stream_header, query,
};

use crate::daemon::{CreateError, Registry};
use crate::limits::ServerLimits;
use crate::seam::{Clock, Tasks};
use crate::shell::{ActorHandle, CommandError, Seams};

/// What every handler shares.
#[derive(Clone)]
pub struct App {
    registry: Registry,
    clock: Arc<dyn Clock>,
    tasks: Tasks,
    limits: Arc<ServerLimits>,
    /// One permit per live stream follower across sessions (`sse_followers`).
    followers: Arc<Semaphore>,
}

impl fmt::Debug for App {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("App").finish_non_exhaustive()
    }
}

impl App {
    #[must_use]
    pub fn new(registry: Registry, seams: Seams, limits: &ServerLimits) -> Self {
        Self {
            registry,
            clock: seams.clock,
            tasks: seams.tasks,
            limits: Arc::new(limits.clone()),
            followers: Arc::new(Semaphore::new(limits.sse_followers)),
        }
    }
}

/// The daemon's routes.
pub fn router(app: App) -> Router {
    let body_limit = app.limits.wire.max_body_bytes;
    Router::new()
        .route("/version", get(version))
        .route("/actors", post(create_actor).get(list_actors))
        .route("/actors/{id}/messages", post(send_message))
        .route("/actors/{id}/approvals", post(approve))
        .route("/actors/{id}/cancel", post(cancel))
        .route("/actors/{id}/compact", post(compact))
        .route("/actors/{id}/stop", post(stop))
        .route("/actors/{id}/stream", get(read_stream))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(app)
}

fn respond(status: StatusCode, headers: &[(header::HeaderName, String)], body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in headers {
        let value = HeaderValue::from_str(value).expect("header values are ASCII we formatted");
        response.headers_mut().insert(name.clone(), value);
    }
    response
}

fn json(status: StatusCode, body: Vec<u8>) -> Response {
    respond(
        status,
        &[(header::CONTENT_TYPE, "application/json".to_owned())],
        Body::from(body),
    )
}

fn rejection(rejection: &Rejection) -> Response {
    json(
        StatusCode::from_u16(rejection.status()).unwrap_or(StatusCode::CONFLICT),
        encode_reply(rejection),
    )
}

fn unavailable(reason: &str) -> Response {
    respond(
        StatusCode::SERVICE_UNAVAILABLE,
        &[(header::CONTENT_TYPE, "text/plain".to_owned())],
        Body::from(reason.to_owned()),
    )
}

const fn created(outcome: WireOutcome) -> StatusCode {
    match outcome {
        WireOutcome::Created => StatusCode::CREATED,
        WireOutcome::Exists => StatusCode::OK,
    }
}

fn command_error(error: &CommandError) -> Response {
    match error {
        CommandError::Rejected(refused) => rejection(refused),
        CommandError::Unavailable(gone) => unavailable(&gone.to_string()),
    }
}

async fn version() -> Response {
    json(
        StatusCode::OK,
        serde_json::json!({"version": env!("CARGO_PKG_VERSION")})
            .to_string()
            .into_bytes(),
    )
}

async fn create_actor(State(app): State<App>, body: Bytes) -> Response {
    let command = match CreateActor::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match app.registry.create(command.actor_id).await {
        Ok((outcome, _)) => json(
            created(outcome),
            encode_reply(&CreateActorReply { outcome }),
        ),
        Err(CreateError::Full { limit }) => rejection(&Rejection::TooManyActors { limit }),
        Err(
            error @ (CreateError::Log(_) | CreateError::Unborn { .. } | CreateError::Unavailable),
        ) => unavailable(&error.to_string()),
    }
}

/// Every born actor, in id order, from each one's live session. One whose
/// session stopped meanwhile is left out.
async fn list_actors(State(app): State<App>) -> Response {
    let mut actors = Vec::new();
    for listed in app.registry.list().await {
        let Some(handle) = app.registry.lookup(listed.actor).await else {
            continue;
        };
        if let Ok(Some(summary)) = handle.summary().await {
            actors.push(summary);
        }
    }
    json(StatusCode::OK, encode_reply(&ActorList { actors }))
}

async fn session(app: &App, id: &str) -> Result<ActorHandle, Box<Response>> {
    let actor_id = ActorId::parse(id).map_err(|error| {
        Box::new(rejection(&Rejection::Malformed {
            reason: format!("actor id: {error}"),
        }))
    })?;
    app.registry
        .lookup(actor_id)
        .await
        .ok_or_else(|| Box::new(rejection(&Rejection::UnknownActor { actor_id })))
}

async fn send_message(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let command = match SendMessage::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match handle.send_message(command).await {
        Ok(reply) => json(created(reply.outcome), encode_reply(&reply)),
        Err(error) => command_error(&error),
    }
}

async fn approve(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let command = match Approve::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match handle.approve(command).await {
        Ok(reply) => json(created(reply.outcome), encode_reply(&reply)),
        Err(error) => command_error(&error),
    }
}

async fn cancel(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let command = match Cancel::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match handle.cancel(command).await {
        Ok(reply) => json(created(reply.outcome), encode_reply(&reply)),
        Err(error) => command_error(&error),
    }
}

async fn compact(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let command = match Compact::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match handle.compact(command).await {
        Ok(reply) => json(created(reply.outcome), encode_reply(&reply)),
        Err(error) => command_error(&error),
    }
}

async fn stop(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let command = match StopActor::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match handle.stop_actor(command).await {
        Ok(reply) => json(created(reply.outcome), encode_reply(&reply)),
        Err(error) => command_error(&error),
    }
}

/// The offset a stream read starts at: the query's one parameter.
fn parse_offset(raw: Option<&str>) -> Result<Offset, String> {
    let mut offset = None;
    for pair in raw
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == query::OFFSET {
            offset = Some(Offset::parse(value).map_err(|error| format!("offset: {error}"))?);
        }
    }
    offset.ok_or_else(|| "offset is required".to_owned())
}

async fn read_stream(
    State(app): State<App>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    match parse_offset(raw.as_deref()) {
        Ok(from) => live(&app, handle, from),
        Err(reason) => rejection(&Rejection::Malformed { reason }),
    }
}

/// The slots one live follower holds: one of its session's and one of the
/// daemon's. Both are released when the follower ends.
struct Slots {
    _session: OwnedSemaphorePermit,
    _daemon: OwnedSemaphorePermit,
}

fn live(app: &App, handle: ActorHandle, from: Offset) -> Response {
    let Some(session_slot) = handle.follower() else {
        return rejection(&Rejection::TooManyFollowers {
            actor_id: handle.actor(),
            limit: app.limits.sse_followers_per_actor,
        });
    };
    let Ok(daemon_slot) = Arc::clone(&app.followers).try_acquire_owned() else {
        return rejection(&Rejection::TooManyFollowers {
            actor_id: handle.actor(),
            limit: app.limits.sse_followers,
        });
    };
    let (sender, receiver) = mpsc::channel(app.limits.sse_queue);
    app.tasks.spawn(follow(
        handle,
        from,
        sender,
        Arc::clone(&app.clock),
        Arc::clone(&app.limits),
        Slots {
            _session: session_slot,
            _daemon: daemon_slot,
        },
    ));
    respond(
        StatusCode::OK,
        &[
            (header::CONTENT_TYPE, "text/event-stream".to_owned()),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
            (
                header::HeaderName::from_static(stream_header::UI_MESSAGE_STREAM),
                stream_header::UI_MESSAGE_STREAM_VERSION.to_owned(),
            ),
        ],
        Body::from_stream(Chunks { receiver }),
    )
}

/// Feeds one live response for at most its lifetime, then ends it. Its
/// follower `slots` are released then, or earlier when the client goes away
/// (noticed at the next send: a commit or a keep-alive) or the session
/// stops.
async fn follow(
    handle: ActorHandle,
    from: Offset,
    sender: mpsc::Sender<Bytes>,
    clock: Arc<dyn Clock>,
    limits: Arc<ServerLimits>,
    slots: Slots,
) {
    let _slots = slots;
    tokio::select! {
        biased;
        () = clock.sleep(limits.sse_lifetime) => {}
        () = feed(&handle, from, &sender, clock.as_ref(), &limits) => {}
    }
}

/// Each read as a `data` event (when it has chunks) and a `control` event,
/// then waits for the next commit, with keep-alives while idle.
async fn feed(
    handle: &ActorHandle,
    from: Offset,
    sender: &mpsc::Sender<Bytes>,
    clock: &dyn Clock,
    limits: &ServerLimits,
) {
    let committed = handle.committed();
    let mut from = from;
    loop {
        let Ok(read) = handle.read(from).await else {
            return;
        };
        let mut text = String::new();
        if !read.chunks.is_empty() {
            StreamEvent::Data(read.chunks)
                .to_sse()
                .encode_into(&mut text);
        }
        StreamEvent::Control(Control {
            stream_next_offset: read.next,
            up_to_date: read.freshness,
        })
        .to_sse()
        .encode_into(&mut text);
        if sender.send(Bytes::from(text)).await.is_err() {
            return;
        }
        from = read.next;
        if matches!(read.freshness, Freshness::Behind) {
            continue;
        }
        loop {
            tokio::select! {
                biased;
                past = committed.past(read.next.seq()) => {
                    if past.is_err() {
                        return;
                    }
                    break;
                }
                () = clock.sleep(limits.sse_keep_alive) => {
                    if sender.send(Bytes::from_static(KEEP_ALIVE.as_bytes())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// A bounded channel as a response body stream.
struct Chunks {
    receiver: mpsc::Receiver<Bytes>,
}

impl futures_core::Stream for Chunks {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx).map(|chunk| chunk.map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use hyper_util::client::legacy::connect::HttpConnector;
    use tokio::sync::{Semaphore, mpsc};
    use verified_domain::Millis;
    use verified_domain::id::{ActorId, UnixMillis};
    use verified_wire::stream::Offset;

    use super::{Slots, follow};
    use crate::daemon;
    use crate::env::NoEnv;
    use crate::limits::ServerLimits;
    use crate::log::{MemDisk, MemLogStore};
    use crate::model::anthropic::{AnthropicConfig, AnthropicModel, ApiKey};
    use crate::seam::{Clock, OsEntropy, SystemClock, Tasks};
    use crate::shell::{Planted, Seams, ShellConfig};

    #[tokio::test]
    async fn a_follower_ends_within_its_lifetime_though_the_client_stops_reading() {
        let limits = ServerLimits {
            sse_lifetime: Millis::new(100),
            sse_keep_alive: Millis::new(10),
            ..ServerLimits::v0()
        };
        let tasks = Tasks::new();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let seams = Seams {
            // Never called: the test sends no message.
            model: Arc::new(
                AnthropicModel::new(
                    HttpConnector::new(),
                    AnthropicConfig {
                        base: "http://127.0.0.1:9".parse().expect("a uri"),
                        api_key: ApiKey::new("unused".to_owned()),
                        model: "fake".to_owned(),
                    },
                    &limits,
                    Arc::clone(&clock),
                )
                .expect("a client"),
            ),
            env: Arc::new(NoEnv),
            clock: Arc::clone(&clock),
            entropy: Arc::new(OsEntropy),
            tasks: tasks.clone(),
        };
        let config = Arc::new(ShellConfig {
            limits: limits.clone(),
            model: "fake".to_owned(),
            system: "You are a test.".to_owned(),
            planted: Planted::None,
        });
        let store = Arc::new(MemLogStore::new(
            MemDisk::new(),
            limits.log_line_bytes,
            limits.log_file_bytes,
        ));
        let (fatal, _failed) = mpsc::channel(1);
        let registry = daemon::boot(store, seams, config, fatal)
            .await
            .expect("the daemon boots");
        let session = ActorId::from_parts(
            UnixMillis::new(1_700_000_000_000).expect("a time"),
            &[7; 10],
        );
        let (_, handle) = registry.create(session).await.expect("created");
        // The first read fills the queue, and nobody reads it: the next
        // keep-alive's send waits forever.
        let (sender, _unread) = mpsc::channel(1);
        let slots = Arc::new(Semaphore::new(2));
        let follower = follow(
            handle,
            Offset::BEGINNING,
            sender,
            Arc::clone(&clock),
            Arc::new(limits),
            Slots {
                _session: Arc::clone(&slots).try_acquire_owned().expect("a slot"),
                _daemon: Arc::clone(&slots).try_acquire_owned().expect("a slot"),
            },
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "a test's own deadline on waiting, not a timeout inside the code under test"
        )]
        let ended = tokio::time::timeout(Duration::from_secs(2), follower).await;
        assert!(
            ended.is_ok(),
            "the follower outlived its 100 ms lifetime by 2 s"
        );
        tasks.shutdown().cancel();
    }
}
