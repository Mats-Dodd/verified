//! The daemon client: posts commands and follows the stream.
//!
//! Every exchange runs on a fresh connection from the [`Dialer`] seam, with
//! hyper's HTTP/1 connection driven in the same task (no executor, no
//! spawns). Commands retry on the [`Limits::command_retry`] schedule when
//! delivery fails in transit, resending the same body and so the same
//! client-minted id; the daemon answers a repeat with "exists". The stream is
//! followed with Durable Streams in SSE mode and resumes from the last
//! control offset after a drop, on the [`Limits::reconnect`] schedule.
//!
//! # Abandoning exchanges (Law 7)
//!
//! The design's seventh law forbids cancelling a *side effect* by dropping
//! its future. Nothing the client does is one: every exchange with the daemon
//! is a network round trip carrying an id the daemon dedups (the first law:
//! a repeat is answered "exists"), or a read of an append-only stream. So
//! the client does drop futures: an exchange that outlives [`Limits::exchange_timeout`], a stream
//! that stays silent past [`Limits::stream_idle`], a command or follower
//! stopped at shutdown. If an abandoned POST landed, the retry (same body,
//! same id) is answered "exists"; if an abandoned read had data in flight,
//! the next read asks for it again from the last delivered offset.
//!
//! No client *state* depends on a dropped future finishing. The follower's
//! only state is its offset, advanced only after a read has been
//! handed to the inbox; every future that touches it is either awaited to
//! the end or dropped on the way to the follower returning, so a
//! half-finished read is never resumed from. The app's own state changes
//! only in `update`, from messages that did arrive.

use std::sync::Arc;

use http_body_util::{BodyExt as _, Full};
use hyper::body::{Body, Bytes};
use hyper::client::conn::http1;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::Millis;
use verified_domain::id::{ActorId, CallId, MessageId, TurnId};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{
    ActorList, Approve, ApproveReply, Cancel, CancelReply, CreateActor, CreateActorReply,
    DecodeError, Rejection, SendMessage, SendMessageReply, StopActor, StopReply, decode_reply,
};
use verified_wire::sse::{SseDecoder, SseError};
use verified_wire::stream::{Control, Offset, StreamError, StreamEvent, query};

use crate::Limits;
use crate::seam::{Clock, Dialer};

/// Why an exchange with the daemon failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the daemon: {0}")]
    Connect(#[source] std::io::Error),
    #[error("connection to the daemon failed: {0}")]
    Http(#[from] hyper::Error),
    #[error("the daemon answered {status}")]
    Status { status: u16 },
    #[error(transparent)]
    Rejected(Rejection),
    #[error("the daemon's reply did not parse: {0}")]
    Reply(#[from] DecodeError),
    #[error("the daemon's reply is longer than {limit} bytes")]
    TooLarge { limit: usize },
    #[error("broken stream: {0}")]
    Stream(#[from] StreamError),
    #[error("a read carried a second data event before its control event")]
    UnpairedData,
    #[error("the daemon closed the stream before any read")]
    ClosedEarly,
    #[error("a read from offset {from} ended at {next} with {chunks} chunks")]
    Offset {
        from: Offset,
        next: Offset,
        chunks: usize,
    },
    #[error("no answer from the daemon within {}ms", .after.get())]
    TimedOut { after: Millis },
}

impl From<SseError> for ClientError {
    fn from(error: SseError) -> Self {
        Self::Stream(StreamError::Sse(error))
    }
}

/// Whether trying again might help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// A transport failure: the same request may succeed later.
    Retry,
    /// The daemon answered, or answered nonsense: retrying repeats it.
    GiveUp,
}

impl ClientError {
    #[must_use]
    pub const fn recovery(&self) -> Recovery {
        match self {
            Self::Connect(_) | Self::Http(_) | Self::ClosedEarly | Self::TimedOut { .. } => {
                Recovery::Retry
            }
            Self::Status { status } if *status >= 500 => Recovery::Retry,
            Self::Rejected(rejection) => match rejection {
                // The daemon is at capacity for now; the same request may
                // fit once an actor or a reader goes away.
                Rejection::TooManyActors { .. } | Rejection::TooManyFollowers { .. } => {
                    Recovery::Retry
                }
                Rejection::Malformed { .. }
                | Rejection::UnknownActor { .. }
                | Rejection::MessageExistsWithDifferentText { .. }
                | Rejection::NoPendingApproval { .. }
                | Rejection::ApprovalExistsWithDifferentVerdict { .. }
                | Rejection::TurnNotRunning { .. }
                | Rejection::ActorStopped { .. }
                | Rejection::ActorFull { .. } => Recovery::GiveUp,
            },
            Self::Status { .. }
            | Self::Reply(_)
            | Self::TooLarge { .. }
            | Self::Stream(_)
            | Self::UnpairedData
            | Self::Offset { .. } => Recovery::GiveUp,
        }
    }
}

/// A command for the daemon. Ids inside were minted by the client and are
/// reused on every retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    CreateActor(CreateActor),
    /// Lists the daemon's actors. A read: never retried, the next refresh
    /// asks again.
    ListActors,
    SendMessage {
        actor: ActorId,
        body: SendMessage,
    },
    Approve {
        actor: ActorId,
        body: Approve,
    },
    /// Cancels `turn`. Always names the turn: a cancel meant for one turn
    /// must never stop the next.
    Cancel {
        actor: ActorId,
        turn: TurnId,
    },
    /// Stops `actor` and every actor it spawned.
    Stop {
        actor: ActorId,
        body: StopActor,
    },
}

/// The daemon's answer to a [`Command`], after any retries.
#[derive(Debug)]
pub enum Answer {
    ActorCreated {
        actor: ActorId,
        result: Result<CreateActorReply, ClientError>,
    },
    Actors(Result<ActorList, ClientError>),
    MessageSent {
        message_id: MessageId,
        result: Result<SendMessageReply, ClientError>,
    },
    Approved {
        call_id: CallId,
        result: Result<ApproveReply, ClientError>,
    },
    Cancelled {
        turn_id: TurnId,
        result: Result<CancelReply, ClientError>,
    },
    Stopped {
        stop_id: MessageId,
        result: Result<StopReply, ClientError>,
    },
}

/// What following a stream reports.
#[derive(Debug)]
pub enum StreamUpdate {
    /// A live read is open; what follows the requested offset comes next.
    Connected,
    /// One read: the offset it started at, its chunks in order, and the
    /// control event after them. `from` is where the previous read delivered
    /// to this follower ended (or the follower's starting offset), and
    /// `control.stream_next_offset` is where the next one starts: reads tile
    /// the stream with no gap and no overlap.
    Read {
        from: Offset,
        chunks: Vec<UiChunk>,
        control: Control,
    },
    /// The connection failed; `retry` is when the next attempt starts, or
    /// `None` when the reconnect schedule is spent and following stopped.
    Disconnected {
        error: ClientError,
        retry: Option<Millis>,
    },
    /// The daemon refused the read or sent a broken stream; following stopped.
    Ended { error: ClientError },
}

/// How a follower ended without being told to stop.
enum LiveEnd {
    /// The daemon closed the connection, as it does periodically.
    ServerClosed,
    /// The receiving end of the inbox is gone.
    InboxClosed,
}

/// Whether a live read delivered anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    None,
    Made,
}

/// The HTTP client of one daemon.
#[derive(Debug, Clone)]
pub struct Client {
    dialer: Arc<dyn Dialer>,
    clock: Arc<dyn Clock>,
    limits: Limits,
}

type Response = (StatusCode, Vec<u8>);

impl Client {
    #[must_use]
    pub fn new(dialer: Arc<dyn Dialer>, clock: Arc<dyn Clock>, limits: Limits) -> Self {
        Self {
            dialer,
            clock,
            limits,
        }
    }

    /// Sends `command`, retrying transport failures on the command schedule.
    pub async fn send(&self, command: Command) -> Answer {
        match command {
            Command::CreateActor(body) => Answer::ActorCreated {
                actor: body.actor_id,
                result: self.post("/actors".to_owned(), body.encode()).await,
            },
            Command::ListActors => Answer::Actors(self.get("/actors").await),
            Command::SendMessage { actor, body } => Answer::MessageSent {
                message_id: body.message_id,
                result: self
                    .post(format!("/actors/{actor}/messages"), body.encode())
                    .await,
            },
            Command::Approve { actor, body } => Answer::Approved {
                call_id: body.call_id.clone(),
                result: self
                    .post(format!("/actors/{actor}/approvals"), body.encode())
                    .await,
            },
            Command::Cancel { actor, turn } => Answer::Cancelled {
                turn_id: turn,
                result: self
                    .post(
                        format!("/actors/{actor}/cancel"),
                        Cancel { turn_id: turn }.encode(),
                    )
                    .await,
            },
            Command::Stop { actor, body } => Answer::Stopped {
                stop_id: body.stop_id,
                result: self
                    .post(format!("/actors/{actor}/stop"), body.encode())
                    .await,
            },
        }
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: String,
        body: Vec<u8>,
    ) -> Result<T, ClientError> {
        let body = Bytes::from(body);
        let mut attempt: u32 = 0;
        loop {
            let request = self.request(Method::POST, &path, body.clone());
            let result = self
                .exchange(request)
                .await
                .and_then(|response| self.reply(response));
            match result {
                Ok(reply) => return Ok(reply),
                Err(error) => match (error.recovery(), self.limits.command_retry.delay(attempt)) {
                    (Recovery::Retry, Some(delay)) => {
                        self.clock.sleep(delay).await;
                        attempt = attempt.saturating_add(1);
                    }
                    (Recovery::Retry, None) | (Recovery::GiveUp, Some(_) | None) => {
                        return Err(error);
                    }
                },
            }
        }
    }

    /// One read, once: a failed read is asked again by whoever wants it.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let request = self.request(Method::GET, path, Bytes::new());
        self.exchange(request)
            .await
            .and_then(|response| self.reply(response))
    }

    fn reply<T: DeserializeOwned>(&self, (status, body): Response) -> Result<T, ClientError> {
        let wire = &self.limits.wire;
        if status.is_success() {
            return Ok(decode_reply(&body, wire)?);
        }
        // Refusals come as 4xx, and as 503 when the daemon is at capacity.
        if let Ok(rejection) = decode_reply::<Rejection>(&body, wire) {
            return Err(ClientError::Rejected(rejection));
        }
        Err(ClientError::Status {
            status: status.as_u16(),
        })
    }

    fn request(&self, method: Method, path: &str, body: Bytes) -> Request<Full<Bytes>> {
        let builder = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, self.dialer.authority());
        let builder = if body.is_empty() {
            builder
        } else {
            builder.header(hyper::header::CONTENT_TYPE, "application/json")
        };
        builder
            .body(Full::new(body))
            .expect("paths, methods and headers built here are valid")
    }

    /// `work`, or [`ClientError::TimedOut`] once `limit` passes. A dead
    /// connection never answers, so every wait on the daemon is bounded; what
    /// is dropped is a read, never a durable step.
    async fn within<T>(
        &self,
        limit: Millis,
        work: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        tokio::select! {
            biased;
            result = work => result,
            () = self.clock.sleep(limit) => Err(ClientError::TimedOut { after: limit }),
        }
    }

    /// One request on a fresh connection; the body is read up to the limit.
    async fn exchange(&self, request: Request<Full<Bytes>>) -> Result<Response, ClientError> {
        let exchange = async {
            let stream = self.dialer.dial().await.map_err(ClientError::Connect)?;
            let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await?;
            let limit = self.limits.wire.max_body_bytes;
            let work = async move {
                let response = sender.send_request(request).await?;
                let status = response.status();
                let body = read_body(response.into_body(), limit).await?;
                Ok((status, body))
            };
            let (result, _connection) = tokio::join!(work, connection);
            result
        };
        self.within(self.limits.exchange_timeout, exchange).await
    }

    /// Follows `actor`'s stream from `from` until `stop` is cancelled, the
    /// daemon refuses it, or the reconnect schedule is spent. Each update is
    /// wrapped by `wrap` and delivered to `inbox`, waiting while it is full.
    pub async fn follow<T, W>(
        &self,
        actor: ActorId,
        from: Offset,
        inbox: &mpsc::Sender<T>,
        wrap: W,
        stop: &CancellationToken,
    ) where
        T: Send,
        W: Fn(StreamUpdate) -> T + Sync,
    {
        let mut offset = from;
        let mut failures: u32 = 0;
        loop {
            let mut progress = Progress::None;
            let read = tokio::select! {
                biased;
                () = stop.cancelled() => return,
                read = self.read_live(actor, &mut offset, &mut progress, inbox, &wrap) => read,
            };
            if progress == Progress::Made {
                failures = 0;
            }
            let error = match read {
                Ok(LiveEnd::InboxClosed) => return,
                Ok(LiveEnd::ServerClosed) if progress == Progress::Made => continue,
                Ok(LiveEnd::ServerClosed) => ClientError::ClosedEarly,
                Err(error) => error,
            };
            if error.recovery() == Recovery::GiveUp {
                // Delivery failing means the program is stopping anyway.
                let _delivered = inbox.send(wrap(StreamUpdate::Ended { error })).await;
                return;
            }
            let retry = self.limits.reconnect.delay(failures);
            failures = failures.saturating_add(1);
            let update = StreamUpdate::Disconnected { error, retry };
            if inbox.send(wrap(update)).await.is_err() {
                return;
            }
            let Some(delay) = retry else { return };
            tokio::select! {
                biased;
                () = stop.cancelled() => return,
                () = self.clock.sleep(delay) => {}
            }
        }
    }

    /// One live read: delivers each read's chunks once the control event
    /// after them arrives, so a drop mid-read redelivers nothing twice.
    async fn read_live<T, W>(
        &self,
        actor: ActorId,
        offset: &mut Offset,
        progress: &mut Progress,
        inbox: &mpsc::Sender<T>,
        wrap: &W,
    ) -> Result<LiveEnd, ClientError>
    where
        T: Send,
        W: Fn(StreamUpdate) -> T + Sync,
    {
        let path = format!("/actors/{actor}/stream?{}={offset}", query::OFFSET);
        let request = self.request(Method::GET, &path, Bytes::new());
        let opening = async {
            let stream = self.dialer.dial().await.map_err(ClientError::Connect)?;
            Ok(http1::handshake(TokioIo::new(stream)).await?)
        };
        let (mut sender, connection) = self.within(self.limits.exchange_timeout, opening).await?;
        let wire = self.limits.wire;
        let idle = self.limits.stream_idle;
        let work = async move {
            let response = self
                .within(self.limits.exchange_timeout, async {
                    Ok(sender.send_request(request).await?)
                })
                .await?;
            let status = response.status();
            let mut body = response.into_body();
            if !status.is_success() {
                let bytes = read_body(body, wire.max_body_bytes).await?;
                return Err(self
                    .reply::<serde_json::Value>((status, bytes))
                    .err()
                    .unwrap_or(ClientError::Status {
                        status: status.as_u16(),
                    }));
            }
            if inbox.send(wrap(StreamUpdate::Connected)).await.is_err() {
                return Ok(LiveEnd::InboxClosed);
            }
            let mut decoder = SseDecoder::new(&wire);
            // The server sends each read as at most one data event, then
            // its control event: so at most one event is ever pending.
            let mut pending: Option<Vec<UiChunk>> = None;
            while let Some(frame) = self.within(idle, async { Ok(body.frame().await) }).await? {
                let Ok(bytes) = frame?.into_data() else {
                    continue;
                };
                for event in decoder.feed(&bytes) {
                    match StreamEvent::from_sse(&event?)? {
                        StreamEvent::Data(chunks) => {
                            if pending.replace(chunks).is_some() {
                                return Err(ClientError::UnpairedData);
                            }
                        }
                        StreamEvent::Control(control) => {
                            let from = *offset;
                            let next = control.stream_next_offset;
                            let chunks = pending.take().unwrap_or_default();
                            // A read ends at or past its start, and one with
                            // chunks covers at least one record.
                            if next < from || (next == from && !chunks.is_empty()) {
                                return Err(ClientError::Offset {
                                    from,
                                    next,
                                    chunks: chunks.len(),
                                });
                            }
                            let update = StreamUpdate::Read {
                                from,
                                chunks,
                                control,
                            };
                            if inbox.send(wrap(update)).await.is_err() {
                                return Ok(LiveEnd::InboxClosed);
                            }
                            // Delivered: the next read starts exactly where
                            // this one ended. Chunks still pending when the
                            // connection drops were never delivered, so the
                            // next connection asks for them again.
                            *offset = next;
                            *progress = Progress::Made;
                        }
                    }
                }
            }
            Ok(LiveEnd::ServerClosed)
        };
        let (result, _connection) = tokio::join!(work, connection);
        result
    }
}

/// Reads a whole body, refusing one longer than `limit` bytes.
async fn read_body<B>(mut body: B, limit: usize) -> Result<Vec<u8>, ClientError>
where
    B: Body<Data = Bytes, Error = hyper::Error> + Unpin,
{
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(chunk) = frame?.into_data() else {
            continue;
        };
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ClientError::TooLarge { limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
