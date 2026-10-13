//! HTTP routes: commands in, the Durable Stream out.
//!
//! - `POST /sessions`, `/sessions/{id}/messages`, `/approvals`, `/cancel`:
//!   commands with client-minted ids; a repeat answers `exists`
//!   (`TigerBeetle`), a conflicting repeat is a 409 rejection.
//! - `GET /sessions/{id}/stream?offset=…`: a catch-up read (Durable
//!   Streams §5.6), cacheable by range with an `etag`.
//! - `GET /sessions/{id}/stream?offset=…&live=sse&cursor=…`: catch-up, then
//!   every append, each read followed by a `control` event (§5.8); closed
//!   after `ServerLimits::sse_lifetime` so clients reconnect from the last
//!   offset.
//! - `HEAD /sessions/{id}/stream`: the next offset (§5.5).
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
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use verified_domain::id::SessionId;
use verified_wire::command::{
    Approve, Cancel, CreateSession, CreateSessionReply, Outcome as WireOutcome, Rejection,
    SendMessage, encode_reply,
};
use verified_wire::sse::KEEP_ALIVE;
use verified_wire::stream::{
    CATCH_UP_CACHE_CONTROL, Control, Cursor, Freshness, Jitter, NO_STORE, ReadFrom, StreamEvent,
    catch_up_etag, encode_chunks, header as stream_header, query,
};

use crate::daemon::{CreateError, Registry};
use crate::limits::ServerLimits;
use crate::seam::{Clock, Entropy, World, entropy_u64};
use crate::shell::{CommandError, Seams, SessionHandle};

/// What every handler shares.
#[derive(Clone)]
pub struct App {
    registry: Registry,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    world: World,
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
            entropy: seams.entropy,
            world: seams.world,
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
        .route("/sessions", post(create_session))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/approvals", post(approve))
        .route("/sessions/{id}/cancel", post(cancel))
        .route("/sessions/{id}/stream", get(read_stream).head(metadata))
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

async fn create_session(State(app): State<App>, body: Bytes) -> Response {
    let command = match CreateSession::decode(&body, &app.limits.wire) {
        Ok(command) => command,
        Err(error) => return rejection(&Rejection::from(error)),
    };
    match app.registry.create(command.session_id).await {
        Ok((outcome, _)) => json(
            created(outcome),
            encode_reply(&CreateSessionReply { outcome }),
        ),
        Err(CreateError::Full { limit }) => rejection(&Rejection::TooManySessions { limit }),
        Err(error @ (CreateError::Log(_) | CreateError::Unavailable)) => {
            unavailable(&error.to_string())
        }
    }
}

async fn session(app: &App, id: &str) -> Result<SessionHandle, Box<Response>> {
    let session_id = SessionId::parse(id).map_err(|error| {
        Box::new(rejection(&Rejection::Malformed {
            reason: format!("session id: {error}"),
        }))
    })?;
    app.registry
        .lookup(session_id)
        .await
        .ok_or_else(|| Box::new(rejection(&Rejection::SessionNotFound { session_id })))
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

/// Whether a read follows the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Live {
    No,
    Sse,
}

/// The stream query, parsed once.
struct StreamQuery {
    offset: Option<ReadFrom>,
    live: Live,
    cursor: Option<Cursor>,
}

fn parse_query(raw: Option<&str>) -> Result<StreamQuery, String> {
    let mut parsed = StreamQuery {
        offset: None,
        live: Live::No,
        cursor: None,
    };
    for pair in raw
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            query::OFFSET => {
                parsed.offset =
                    Some(ReadFrom::parse(value).map_err(|error| format!("offset: {error}"))?);
            }
            query::LIVE => {
                if value != query::LIVE_SSE {
                    return Err(format!("live: only `{}` is served", query::LIVE_SSE));
                }
                parsed.live = Live::Sse;
            }
            query::CURSOR => {
                parsed.cursor =
                    Some(Cursor::parse(value).map_err(|error| format!("cursor: {error}"))?);
            }
            _ => {}
        }
    }
    Ok(parsed)
}

async fn read_stream(
    State(app): State<App>,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let query = match parse_query(raw.as_deref()) {
        Ok(query) => query,
        Err(reason) => return rejection(&Rejection::Malformed { reason }),
    };
    match query.live {
        Live::Sse => {
            let Some(from) = query.offset else {
                return rejection(&Rejection::Malformed {
                    reason: "offset is required for live reads".to_owned(),
                });
            };
            live(&app, handle, from, query.cursor)
        }
        Live::No => {
            let from = query
                .offset
                .unwrap_or(ReadFrom::Offset(verified_wire::stream::Offset::BEGINNING));
            catch_up(handle, from, &headers).await
        }
    }
}

async fn catch_up(handle: SessionHandle, from: ReadFrom, headers: &HeaderMap) -> Response {
    let Ok(read) = handle.read(from).await else {
        return unavailable("the session is not running");
    };
    let mut response_headers = vec![
        (header::CONTENT_TYPE, "application/json".to_owned()),
        (
            header::HeaderName::from_static(stream_header::STREAM_NEXT_OFFSET),
            read.next.to_string(),
        ),
    ];
    if matches!(read.freshness, Freshness::UpToDate) {
        response_headers.push((
            header::HeaderName::from_static(stream_header::STREAM_UP_TO_DATE),
            "true".to_owned(),
        ));
    }
    match from {
        ReadFrom::Now => {
            response_headers.push((header::CACHE_CONTROL, NO_STORE.to_owned()));
        }
        ReadFrom::Offset(offset) => {
            let etag = catch_up_etag(handle.session(), offset, read.next);
            let matched = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                == Some(etag.as_str());
            if matched {
                return respond(
                    StatusCode::NOT_MODIFIED,
                    &[(header::ETAG, etag)],
                    Body::empty(),
                );
            }
            response_headers.push((header::CACHE_CONTROL, CATCH_UP_CACHE_CONTROL.to_owned()));
            response_headers.push((header::ETAG, etag));
        }
    }
    respond(
        StatusCode::OK,
        &response_headers,
        Body::from(encode_chunks(&read.chunks)),
    )
}

async fn metadata(State(app): State<App>, Path(id): Path<String>) -> Response {
    let handle = match session(&app, &id).await {
        Ok(handle) => handle,
        Err(response) => return *response,
    };
    let Ok(read) = handle.read(ReadFrom::Now).await else {
        return unavailable("the session is not running");
    };
    respond(
        StatusCode::OK,
        &[
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (header::CACHE_CONTROL, NO_STORE.to_owned()),
            (
                header::HeaderName::from_static(stream_header::STREAM_NEXT_OFFSET),
                read.next.to_string(),
            ),
        ],
        Body::empty(),
    )
}

/// The slots one live follower holds: one of its session's and one of the
/// daemon's. Both are released when the follower ends.
struct Slots {
    _session: OwnedSemaphorePermit,
    _daemon: OwnedSemaphorePermit,
}

fn live(app: &App, handle: SessionHandle, from: ReadFrom, echoed: Option<Cursor>) -> Response {
    let Some(session_slot) = handle.follower() else {
        return rejection(&Rejection::TooManyFollowers {
            session_id: handle.session(),
            limit: app.limits.sse_followers_per_session,
        });
    };
    let Ok(daemon_slot) = Arc::clone(&app.followers).try_acquire_owned() else {
        return rejection(&Rejection::TooManyFollowers {
            session_id: handle.session(),
            limit: app.limits.sse_followers,
        });
    };
    let cursor = Cursor::next(
        app.clock.now(),
        echoed,
        Jitter::from_entropy(entropy_u64(app.entropy.as_ref())),
    );
    let (sender, receiver) = mpsc::channel(app.limits.sse_queue);
    app.world.spawn(follow(
        handle,
        from,
        cursor,
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
    handle: SessionHandle,
    from: ReadFrom,
    cursor: Cursor,
    sender: mpsc::Sender<Bytes>,
    clock: Arc<dyn Clock>,
    limits: Arc<ServerLimits>,
    slots: Slots,
) {
    let _slots = slots;
    tokio::select! {
        biased;
        () = clock.sleep(limits.sse_lifetime) => {}
        () = feed(&handle, from, cursor, &sender, clock.as_ref(), &limits) => {}
    }
}

/// Each read as a `data` event (when it has chunks) and a `control` event,
/// then waits for the next commit, with keep-alives while idle.
async fn feed(
    handle: &SessionHandle,
    from: ReadFrom,
    cursor: Cursor,
    sender: &mpsc::Sender<Bytes>,
    clock: &dyn Clock,
    limits: &ServerLimits,
) {
    let tail = handle.tail();
    let mut position = from;
    loop {
        let Ok(read) = handle.read(position).await else {
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
            stream_cursor: cursor,
            up_to_date: read.freshness,
        })
        .to_sse()
        .encode_into(&mut text);
        if sender.send(Bytes::from(text)).await.is_err() {
            return;
        }
        position = ReadFrom::Offset(read.next);
        if matches!(read.freshness, Freshness::Behind) {
            continue;
        }
        loop {
            tokio::select! {
                biased;
                past = tail.past(read.next.seq()) => {
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
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{Semaphore, mpsc};
    use verified_domain::Millis;
    use verified_domain::id::{SessionId, UnixMillis};
    use verified_wire::stream::{Cursor, Jitter, Offset, ReadFrom};

    use super::{Slots, follow};
    use crate::daemon;
    use crate::env::LocalEnv;
    use crate::limits::ServerLimits;
    use crate::log::{MemDisk, MemLogStore};
    use crate::model::fake::FakeModel;
    use crate::seam::{Clock, OsEntropy, SystemClock, World};
    use crate::shell::{Planted, Seams, ShellConfig};

    #[tokio::test]
    async fn a_follower_ends_within_its_lifetime_though_the_client_stops_reading() {
        let limits = ServerLimits {
            sse_lifetime: Millis::new(100),
            sse_keep_alive: Millis::new(10),
            ..ServerLimits::v0()
        };
        let world = World::new();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let seams = Seams {
            model: Arc::new(FakeModel::new(
                Arc::clone(&clock),
                limits.model_parts_per_call,
            )),
            env: Arc::new(LocalEnv::new(
                PathBuf::from("."),
                Arc::clone(&clock),
                world.groups().clone(),
            )),
            clock: Arc::clone(&clock),
            entropy: Arc::new(OsEntropy),
            world: world.clone(),
        };
        let config = Arc::new(ShellConfig {
            limits: limits.clone(),
            model: "fake".to_owned(),
            system: "You are a test.".to_owned(),
            planted: Planted::default(),
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
        let session = SessionId::from_parts(
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
            ReadFrom::Offset(Offset::BEGINNING),
            Cursor::next(clock.now(), None, Jitter::from_entropy(0)),
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
        world.shutdown().cancel();
    }
}
