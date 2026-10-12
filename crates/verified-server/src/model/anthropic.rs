//! Anthropic's streaming Messages API over hyper with an injected connector.
//!
//! The request is built from a [`ModelRequest`]; the response's SSE is
//! decoded by verified-wire's [`SseDecoder`] and each Anthropic event is
//! parsed once into [`ModelPart`]s by [`StreamParser`], which the fake model
//! shares so tests exercise the same mapping.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::header::HeaderValue;
use hyper::{Method, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower_service::Service;
use verified_domain::Millis;
use verified_domain::id::{CallId, ToolName};
use verified_wire::WireLimits;
use verified_wire::chunk::FinishReason;
use verified_wire::sse::{SseDecoder, SseEvent};

use super::{
    Content, Message, Model, ModelError, ModelRequest, ResultKind, Role, tool_definitions,
};
use crate::limits::ServerLimits;
use crate::payload::{ModelPart, ToolInput};
use crate::seam::Clock;

/// The `anthropic-version` header every request carries.
pub const API_VERSION: &str = "2023-06-01";

/// The API key, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    #[must_use]
    pub const fn new(key: String) -> Self {
        Self(key)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(..)")
    }
}

/// Where and as whom to call Anthropic: data from main.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// `https://api.anthropic.com`, or a fake host in tests and simulation.
    pub base: Uri,
    pub api_key: ApiKey,
    /// The model id, e.g. `claude-sonnet-4-5`.
    pub model: String,
}

/// The JSON body of a streaming Messages request.
#[must_use]
pub fn request_json(model: &str, request: &ModelRequest) -> Value {
    let messages: Vec<Value> = request.messages.iter().map(message_json).collect();
    json!({
        "model": model,
        "max_tokens": request.max_tokens,
        "system": request.system,
        "messages": messages,
        "tools": tool_definitions(),
        "stream": true,
    })
}

fn message_json(message: &Message) -> Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = message
        .content
        .iter()
        .map(|block| match block {
            Content::Text { text } => json!({"type": "text", "text": text}),
            Content::ToolUse { id, name, input } => {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            Content::ToolResult { id, text, kind } => json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": text,
                "is_error": matches!(kind, ResultKind::Error),
            }),
        })
        .collect();
    json!({"role": role, "content": content})
}

/// The failure an unsuccessful status means.
#[must_use]
pub fn classify_status(status: u16, body: &str) -> ModelError {
    let detail = format!("status {status}: {body}");
    match status {
        429 | 529 => ModelError::Overloaded(detail),
        408 | 500 | 502 | 503 | 504 => ModelError::Network(detail),
        _ => ModelError::Other(detail),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    MessageStart {},
    ContentBlockStart {
        index: u32,
        content_block: BlockStart,
    },
    ContentBlockDelta {
        index: u32,
        delta: Change,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDelta,
    },
    MessageStop {},
    Ping {},
    Error {
        error: ApiError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockStart {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Change {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

enum Block {
    Text,
    Tool {
        id: CallId,
        name: ToolName,
        input: String,
    },
    /// A block type v0 does not use (thinking, server tools): skipped.
    Ignored,
}

/// Whether the response is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    More,
    Done,
}

/// Anthropic stream events in, model parts out.
pub struct StreamParser {
    blocks: BTreeMap<u32, Block>,
    /// The `tool_use` ids seen so far: a response reusing one fails here,
    /// so two live calls never share an id.
    calls: BTreeSet<CallId>,
    stop: FinishReason,
    parts: usize,
    max_parts: usize,
    max_input: usize,
}

impl fmt::Debug for StreamParser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamParser")
            .field("blocks", &self.blocks.len())
            .field("parts", &self.parts)
            .finish_non_exhaustive()
    }
}

fn finish_reason(stop: &str) -> FinishReason {
    match stop {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

impl StreamParser {
    /// A parser for one response of at most `max_parts` parts, whose tool
    /// inputs are at most `max_input` bytes each.
    #[must_use]
    pub const fn new(max_parts: usize, max_input: usize) -> Self {
        Self {
            blocks: BTreeMap::new(),
            calls: BTreeSet::new(),
            stop: FinishReason::Stop,
            parts: 0,
            max_parts,
            max_input,
        }
    }

    fn emit(&mut self, out: &mut Vec<ModelPart>, part: ModelPart) -> Result<(), ModelError> {
        if self.parts >= self.max_parts {
            return Err(ModelError::Other(format!(
                "the response has more than {} parts",
                self.max_parts
            )));
        }
        self.parts = self.parts.saturating_add(1);
        out.push(part);
        Ok(())
    }

    /// Parses one SSE event, appending the parts it means to `out`.
    ///
    /// # Errors
    /// An `error` event, malformed JSON, an invalid tool id or name, or a
    /// response larger than the bounds.
    pub fn event(
        &mut self,
        event: &SseEvent,
        out: &mut Vec<ModelPart>,
    ) -> Result<Progress, ModelError> {
        let parsed: Event = serde_json::from_str(event.payload()).map_err(|error| {
            ModelError::Other(format!("malformed `{}` event: {error}", event.name()))
        })?;
        match parsed {
            Event::MessageStart {} | Event::Ping {} | Event::Unknown => {}
            Event::ContentBlockStart {
                index,
                content_block,
            } => self.start(index, content_block, out)?,
            Event::ContentBlockDelta { index, delta } => self.delta(index, delta, out)?,
            Event::ContentBlockStop { index } => self.stop(index, out)?,
            Event::MessageDelta { delta } => {
                if let Some(stop) = delta.stop_reason {
                    self.stop = finish_reason(&stop);
                }
            }
            Event::MessageStop {} => {
                let finish = ModelPart::Finish {
                    finish_reason: self.stop,
                };
                self.emit(out, finish)?;
                return Ok(Progress::Done);
            }
            Event::Error { error } => {
                let detail = format!("{}: {}", error.kind, error.message);
                return Err(match error.kind.as_str() {
                    "overloaded_error" | "rate_limit_error" => ModelError::Overloaded(detail),
                    "api_error" => ModelError::Network(detail),
                    _ => ModelError::Other(detail),
                });
            }
        }
        Ok(Progress::More)
    }

    fn start(
        &mut self,
        index: u32,
        start: BlockStart,
        out: &mut Vec<ModelPart>,
    ) -> Result<(), ModelError> {
        if self.blocks.contains_key(&index) {
            return Err(ModelError::Other(format!("block {index} started twice")));
        }
        let block = match start {
            BlockStart::Text { text } => {
                self.emit(out, ModelPart::TextStart { block: index })?;
                if !text.is_empty() {
                    self.emit(
                        out,
                        ModelPart::TextDelta {
                            block: index,
                            delta: text,
                        },
                    )?;
                }
                Block::Text
            }
            BlockStart::ToolUse { id, name } => {
                let id = CallId::parse(&id)
                    .map_err(|error| ModelError::Other(format!("tool_use id: {error}")))?;
                let name = ToolName::parse(&name)
                    .map_err(|error| ModelError::Other(format!("tool name: {error}")))?;
                if !self.calls.insert(id.clone()) {
                    return Err(ModelError::Other(format!(
                        "the response reuses tool_use id {id}"
                    )));
                }
                self.emit(
                    out,
                    ModelPart::ToolInputStart {
                        id: id.clone(),
                        tool_name: name.clone(),
                    },
                )?;
                Block::Tool {
                    id,
                    name,
                    input: String::new(),
                }
            }
            BlockStart::Other => Block::Ignored,
        };
        self.blocks.insert(index, block);
        Ok(())
    }

    fn delta(
        &mut self,
        index: u32,
        delta: Change,
        out: &mut Vec<ModelPart>,
    ) -> Result<(), ModelError> {
        let max_input = self.max_input;
        let part = match (self.blocks.get_mut(&index), delta) {
            (None, Change::TextDelta { .. } | Change::InputJsonDelta { .. } | Change::Other) => {
                return Err(ModelError::Other(format!(
                    "delta for unknown block {index}"
                )));
            }
            (Some(Block::Text), Change::TextDelta { text }) => ModelPart::TextDelta {
                block: index,
                delta: text,
            },
            (Some(Block::Tool { id, input, .. }), Change::InputJsonDelta { partial_json }) => {
                if input.len().saturating_add(partial_json.len()) > max_input {
                    return Err(ModelError::Other(format!(
                        "tool input longer than {max_input} bytes"
                    )));
                }
                input.push_str(&partial_json);
                ModelPart::ToolInputDelta {
                    id: id.clone(),
                    delta: partial_json,
                }
            }
            (Some(Block::Ignored), Change::TextDelta { .. } | Change::InputJsonDelta { .. })
            | (Some(Block::Text | Block::Tool { .. } | Block::Ignored), Change::Other) => {
                return Ok(());
            }
            (Some(Block::Text), Change::InputJsonDelta { .. })
            | (Some(Block::Tool { .. }), Change::TextDelta { .. }) => {
                return Err(ModelError::Other(format!(
                    "delta of the wrong kind for block {index}"
                )));
            }
        };
        self.emit(out, part)
    }

    fn stop(&mut self, index: u32, out: &mut Vec<ModelPart>) -> Result<(), ModelError> {
        match self.blocks.get(&index) {
            None => Err(ModelError::Other(format!("stop for unknown block {index}"))),
            Some(Block::Text) => self.emit(out, ModelPart::TextEnd { block: index }),
            Some(Block::Ignored) => Ok(()),
            Some(Block::Tool { id, name, input }) => {
                let (id, name) = (id.clone(), name.clone());
                let input = ToolInput::parse(input)
                    .map_err(|error| ModelError::Other(format!("tool_use {id}: {error}")))?;
                self.emit(out, ModelPart::ToolInputEnd { id: id.clone() })?;
                self.emit(
                    out,
                    ModelPart::ToolCall {
                        tool_call_id: id,
                        tool_name: name,
                        input,
                    },
                )
            }
        }
    }
}

/// The real model: Anthropic over hyper with connector `C`.
///
/// Each call dials a fresh connection and drives it inside the call's own
/// task (`hyper::client::conn::http1`), so the client starts no task of its
/// own: every task the daemon runs goes through the world. Dialing, waiting
/// for the response head and every gap between body frames are bounded by
/// [`ServerLimits`] timeouts on the Clock seam; running past one is a
/// [`ModelError::Network`], which the core retries on its schedule.
pub struct AnthropicModel<C> {
    connector: C,
    clock: Arc<dyn Clock>,
    endpoint: Uri,
    host: HeaderValue,
    config: AnthropicConfig,
    wire: WireLimits,
    max_parts: usize,
    max_error_body: usize,
    connect_timeout: Millis,
    response_timeout: Millis,
    idle_timeout: Millis,
    total_timeout: Millis,
}

impl<C> fmt::Debug for AnthropicModel<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicModel")
            .field("endpoint", &self.endpoint)
            .field("model", &self.config.model)
            .finish_non_exhaustive()
    }
}

/// Why the model client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("the base URL does not join with /v1/messages: {0}")]
    Endpoint(String),
    #[error("no native root certificates: {0}")]
    Roots(#[from] std::io::Error),
}

/// One HTTP/1 connection to the model, driven by the call that opened it.
type Connection<Io> = http1::Connection<Io, Full<Bytes>>;

impl<C> AnthropicModel<C>
where
    C: Service<Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    C::Future: Send,
    C::Error: fmt::Display,
{
    /// A client dialing through `connector`, timing out on `clock`.
    ///
    /// # Errors
    /// A base URL that does not make an endpoint.
    pub fn new(
        connector: C,
        config: AnthropicConfig,
        limits: &ServerLimits,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BuildError> {
        let base = config.base.to_string();
        let endpoint = format!("{}/v1/messages", base.trim_end_matches('/'))
            .parse::<Uri>()
            .map_err(|error| BuildError::Endpoint(error.to_string()))?;
        let host = endpoint
            .authority()
            .and_then(|authority| HeaderValue::from_str(authority.as_str()).ok())
            .ok_or_else(|| BuildError::Endpoint(format!("{endpoint} has no host")))?;
        Ok(Self {
            connector,
            clock,
            endpoint,
            host,
            config,
            wire: limits.wire,
            max_parts: limits.model_parts_per_call,
            max_error_body: limits.model_error_body_bytes,
            connect_timeout: limits.model_connect_timeout,
            response_timeout: limits.model_response_timeout,
            idle_timeout: limits.model_idle_timeout,
            total_timeout: limits.model_total_timeout,
        })
    }

    /// `work`, or a [`ModelError::Network`] naming `what` once `limit`
    /// passes, or [`ModelError::Interrupted`] once `cancel` fires.
    async fn bounded<T>(
        &self,
        what: &str,
        limit: Millis,
        cancel: &CancellationToken,
        work: impl Future<Output = Result<T, ModelError>>,
    ) -> Result<T, ModelError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ModelError::Interrupted),
            result = work => result,
            () = self.clock.sleep(limit) => Err(ModelError::Network(format!(
                "{what} took longer than {} ms",
                limit.get()
            ))),
        }
    }

    /// Dials the endpoint and handshakes HTTP/1 on it.
    async fn connect(
        &self,
    ) -> Result<(http1::SendRequest<Full<Bytes>>, Connection<C::Response>), ModelError> {
        let mut connector = self.connector.clone();
        core::future::poll_fn(|cx| connector.poll_ready(cx))
            .await
            .map_err(|error| ModelError::Network(format!("connector: {error}")))?;
        let io = connector
            .call(self.endpoint.clone())
            .await
            .map_err(|error| ModelError::Network(format!("connect: {error}")))?;
        http1::handshake(io)
            .await
            .map_err(|error| ModelError::Network(format!("handshake: {error}")))
    }

    /// Sends the request and streams the response into `parts`.
    async fn exchange(
        &self,
        mut sender: http1::SendRequest<Full<Bytes>>,
        request: &ModelRequest,
        parts: &mpsc::Sender<ModelPart>,
        cancel: &CancellationToken,
    ) -> Result<(), ModelError> {
        let body = serde_json::to_vec(&request_json(&self.config.model, request))
            .expect("JSON values serialize infallibly");
        let path = self
            .endpoint
            .path_and_query()
            .map_or("/v1/messages", hyper::http::uri::PathAndQuery::as_str);
        let http = hyper::Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(hyper::header::HOST, self.host.clone())
            .header("x-api-key", self.config.api_key.0.as_str())
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| ModelError::Other(format!("request: {error}")))?;
        let response = self
            .bounded("the response head", self.response_timeout, cancel, async {
                sender
                    .send_request(http)
                    .await
                    .map_err(|error| ModelError::Network(error.to_string()))
            })
            .await?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        if status != 200 {
            let text = self.error_body(&mut body, cancel).await?;
            return Err(classify_status(status, &text));
        }
        let mut decoder = SseDecoder::new(&self.wire);
        let mut parser = StreamParser::new(self.max_parts, self.wire.max_part_bytes);
        loop {
            let frame = self
                .bounded("a gap in the response", self.idle_timeout, cancel, async {
                    Ok(body.frame().await)
                })
                .await?;
            let Some(frame) = frame else {
                return Err(ModelError::Network(
                    "the stream ended before message_stop".to_owned(),
                ));
            };
            let frame = frame.map_err(|error| ModelError::Network(error.to_string()))?;
            let Ok(bytes) = frame.into_data() else {
                continue;
            };
            let mut out = Vec::new();
            let mut progress = Progress::More;
            for event in decoder.feed(&bytes) {
                let event = event.map_err(|error| ModelError::Other(error.to_string()))?;
                if matches!(progress, Progress::More) {
                    progress = parser.event(&event, &mut out)?;
                }
            }
            for part in out {
                parts
                    .send(part)
                    .await
                    .map_err(|_closed| ModelError::Interrupted)?;
            }
            if matches!(progress, Progress::Done) {
                return Ok(());
            }
        }
    }

    /// One call: dial, send, and stream the response into `parts`.
    async fn call(
        &self,
        request: &ModelRequest,
        parts: mpsc::Sender<ModelPart>,
        cancel: &CancellationToken,
    ) -> Result<(), ModelError> {
        let (sender, connection) = self
            .bounded("connecting", self.connect_timeout, cancel, self.connect())
            .await?;
        let mut connection = core::pin::pin!(connection);
        let mut exchange = core::pin::pin!(self.exchange(sender, request, &parts, cancel));
        let mut open = true;
        loop {
            tokio::select! {
                biased;
                result = &mut exchange => return result,
                closed = &mut connection, if open => {
                    open = false;
                    if let Err(error) = closed {
                        return Err(ModelError::Network(format!("connection: {error}")));
                    }
                }
            }
        }
    }

    async fn error_body(
        &self,
        body: &mut hyper::body::Incoming,
        cancel: &CancellationToken,
    ) -> Result<String, ModelError> {
        let mut text = Vec::new();
        loop {
            let frame = self
                .bounded(
                    "a gap in the error body",
                    self.idle_timeout,
                    cancel,
                    async { Ok(body.frame().await) },
                )
                .await?;
            let Some(Ok(frame)) = frame else {
                break;
            };
            if let Ok(bytes) = frame.into_data() {
                let room = self.max_error_body.saturating_sub(text.len());
                text.extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
                if text.len() >= self.max_error_body {
                    break;
                }
            }
        }
        Ok(String::from_utf8_lossy(&text).into_owned())
    }
}

impl AnthropicModel<hyper_rustls::HttpsConnector<HttpConnector>> {
    /// The production client: TLS with the OS's root certificates.
    ///
    /// # Errors
    /// No root certificates, or a bad base URL.
    pub fn https(
        config: AnthropicConfig,
        limits: &ServerLimits,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BuildError> {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_or_http()
            .enable_http1()
            .build();
        Self::new(connector, config, limits, clock)
    }
}

#[async_trait]
impl<C> Model for AnthropicModel<C>
where
    C: Service<Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    C::Future: Send,
    C::Error: fmt::Display,
{
    /// Law 7, as clarified: a side effect (a tool, a log append) is never
    /// cancelled by dropping its future, but an in-flight network exchange
    /// that carries an idempotent id may be abandoned. A model call is one:
    /// its step id makes a retry a `reset-step`, and nothing the daemon
    /// holds changes until the parts it already sent are committed by the
    /// shell. So once the exchange ends (done, failed, timed out or
    /// cancelled at a safe point) the connection is dropped mid-flight, and
    /// a connection that fails ends the exchange the same way.
    ///
    /// The whole call is bounded too (`model_total_timeout`): a stream that
    /// only ever sends pings never ends otherwise.
    async fn stream(
        &self,
        request: &ModelRequest,
        parts: mpsc::Sender<ModelPart>,
        cancel: &CancellationToken,
    ) -> Result<(), ModelError> {
        let limit = self.total_timeout;
        tokio::select! {
            biased;
            result = self.call(request, parts, cancel) => result,
            () = self.clock.sleep(limit) => Err(ModelError::Network(format!(
                "the response took longer than {} ms in all",
                limit.get()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use verified_wire::sse::SseEvent;

    use super::{ModelError, Progress, StreamParser, classify_status};
    use crate::payload::ModelPart;

    fn event(name: &str, json: &str) -> SseEvent {
        SseEvent::new(name.to_owned(), json.to_owned()).expect("an event")
    }

    #[test]
    fn tool_use_streams_as_input_then_call() {
        let mut parser = StreamParser::new(100, 1000);
        let mut out = Vec::new();
        for (name, json) in [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"m"}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
        ] {
            assert_eq!(
                parser.event(&event(name, json), &mut out),
                Ok(Progress::More),
                "{name}"
            );
        }
        assert_eq!(
            parser.event(
                &event("message_stop", r#"{"type":"message_stop"}"#),
                &mut out
            ),
            Ok(Progress::Done),
            "message_stop ends it"
        );
        let json = serde_json::to_value(&out).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!([
                {"type":"text-start","block":0u32},
                {"type":"text-delta","block":0u32,"delta":"Hi"},
                {"type":"text-end","block":0u32},
                {"type":"tool-input-start","id":"toolu_1","toolName":"read"},
                {"type":"tool-input-delta","id":"toolu_1","delta":"{\"path\":"},
                {"type":"tool-input-delta","id":"toolu_1","delta":"\"a\"}"},
                {"type":"tool-input-end","id":"toolu_1"},
                {"type":"tool-call","toolCallId":"toolu_1","toolName":"read","input":{"path":"a"}},
                {"type":"finish","finishReason":"tool-calls"}
            ]),
            "AI SDK stream-part vocabulary"
        );
        assert!(
            matches!(out.last(), Some(ModelPart::Finish { .. })),
            "ends with finish"
        );
    }

    #[test]
    fn failures_are_typed() {
        let mut parser = StreamParser::new(100, 1000);
        let mut out = Vec::new();
        assert!(matches!(
            parser.event(
                &event(
                    "error",
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
                ),
                &mut out
            ),
            Err(ModelError::Overloaded(_))
        ));
        assert!(matches!(
            classify_status(529, ""),
            ModelError::Overloaded(_)
        ));
        assert!(matches!(classify_status(503, ""), ModelError::Network(_)));
        assert!(matches!(classify_status(400, ""), ModelError::Other(_)));
        let mut bounded = StreamParser::new(1, 1000);
        assert!(
            bounded
                .event(
                    &event("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"x"}}"#),
                    &mut out
                )
                .is_err(),
            "more parts than the bound"
        );
    }
}
