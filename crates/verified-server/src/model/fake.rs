//! A scripted Anthropic.
//!
//! Ported from q's `test/support/FakeAnthropic.ts` and extended with tool
//! calls. Replies are a pure function of the request, so
//! a retried call gets the same reply (and the same tool call ids).
//!
//! Scripted by the last user message's text:
//!
//! - `slow …`        streams 40 deltas, 50 ms apart (time to cancel)
//! - `fail …`        starts a reply, then sends an `overloaded_error` event
//! - `overloaded …`  answers 529 before streaming anything
//! - `error …`       answers 400 (not retried)
//! - `read P`, `edit P OLD NEW`, `bash CMD`, `bg CMD` (in the background),
//!   `wake MS`, `tasks`, `cancel N`, `spawn NAME PROMPT…`, `send ADDR TEXT…`:
//!   a tool call; several calls are separated by ` ; ` and come in one
//!   response
//! - `reply TEXT…`: `send` TEXT to whoever sent the message; `ping`: reply
//!   `ping`, so two actors that ping each other chatter until a limit
//!   stops them
//! - `A then B …`: a turn of several steps: A's calls, then (once their
//!   results are in) B's, and so on. In a later step `$task` is the task
//!   a background call started, `$child` the actor a spawn made (both read
//!   from the results), and `$from` the sender
//! - an envelope from an agent or from v is scripted by its text, without
//!   the header the history gives it
//! - anything else   streams `Echo: <message>`, a word per delta
//!
//! A last user message of only tool results is answered with the script's
//! next step, or `Result: <first result>` once the script is done.
//!
//! [`router`] serves the script as an HTTP host, for
//! [`super::anthropic::AnthropicModel`] over a socket, or over turmoil in
//! simulation.

use core::fmt;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use verified_domain::Millis;
use verified_wire::sse::SseEvent;

use crate::seam::{Clock, Tasks};

/// The fake's reply to one request.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// An error status before any streaming.
    Status { status: u16, body: Value },
    /// SSE events, `pace` apart.
    Events { events: Vec<Value>, pace: Millis },
}

fn event(kind: &str, fields: &Value) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("type".to_owned(), Value::String(kind.to_owned()));
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            object.insert(key.clone(), value.clone());
        }
    }
    Value::Object(object)
}

fn message_start() -> Value {
    event(
        "message_start",
        &json!({"message": {"id": "msg_fake", "type": "message", "role": "assistant",
            "content": [], "model": "fake", "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
    )
}

fn text_block(index: usize, deltas: &[String]) -> Vec<Value> {
    let mut events = vec![event(
        "content_block_start",
        &json!({"index": index, "content_block": {"type": "text", "text": ""}}),
    )];
    for delta in deltas {
        events.push(event(
            "content_block_delta",
            &json!({"index": index, "delta": {"type": "text_delta", "text": delta}}),
        ));
    }
    events.push(event("content_block_stop", &json!({"index": index})));
    events
}

fn tool_block(index: usize, id: &str, name: &str, input: &Value) -> Vec<Value> {
    let text = input.to_string();
    let middle = text.len().checked_div(2).unwrap_or_default();
    let middle = (0..=middle)
        .rev()
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or_default();
    let (head, tail) = text.split_at(middle);
    vec![
        event(
            "content_block_start",
            &json!({"index": index, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        ),
        event(
            "content_block_delta",
            &json!({"index": index, "delta": {"type": "input_json_delta", "partial_json": head}}),
        ),
        event(
            "content_block_delta",
            &json!({"index": index, "delta": {"type": "input_json_delta", "partial_json": tail}}),
        ),
        event("content_block_stop", &json!({"index": index})),
    ]
}

fn message_end(stop: &str) -> Vec<Value> {
    vec![
        event(
            "message_delta",
            &json!({"delta": {"stop_reason": stop, "stop_sequence": null}, "usage": {"output_tokens": 5}}),
        ),
        event("message_stop", &json!({})),
    ]
}

fn overloaded_error() -> Value {
    json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
}

fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for symbol in text.chars() {
        if symbol == ' ' && !current.is_empty() {
            words.push(core::mem::take(&mut current));
        }
        current.push(symbol);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// The text of `message`, and its tool results' (`content`, `is_error`),
/// in order.
fn said_in(message: &Value) -> (String, Vec<(String, bool)>) {
    let mut text = String::new();
    let mut results = Vec::new();
    if let Some(said) = message.get("content").and_then(Value::as_str) {
        text.push_str(said);
    }
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_result") => results.push((
                block
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )),
            Some(_) | None => {}
        }
    }
    (text, results)
}

/// The script the turn runs: the last user message that says something,
/// and how many steps of it already ran (the assistant messages since).
fn script(messages: &[Value]) -> (String, usize) {
    let mut steps = 0usize;
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            steps = steps.saturating_add(1);
        } else {
            let (text, _) = said_in(message);
            if !text.trim().is_empty() {
                return (text, steps);
            }
        }
    }
    (String::new(), steps)
}

/// The sender a header names: `[from ADDR] …` or `[from ADDR; reply to …] …`.
fn sender(said: &str) -> Option<&str> {
    let rest = said.strip_prefix("[from ")?;
    let end = rest.find([';', ']'])?;
    rest.get(..end)
}

/// The first number after `task ` in `text`.
fn task_in(text: &str) -> Option<&str> {
    text.match_indices("task ").find_map(|(at, word)| {
        let rest = text.get(at.saturating_add(word.len())..)?;
        let digits = rest
            .find(|symbol: char| !symbol.is_ascii_digit())
            .unwrap_or(rest.len());
        rest.get(..digits).filter(|digits| !digits.is_empty())
    })
}

/// The address after `spawned NAME at ` in `text`.
fn child_in(text: &str) -> Option<&str> {
    let rest = text.get(text.find("spawned ")?..)?;
    let rest = rest.get(rest.find(" at ")?.saturating_add(4)..)?;
    rest.split_whitespace().next()
}

/// `step` with its placeholders filled from `results` and the sender.
fn filled(step: &str, results: &[(String, bool)], from: Option<&str>) -> String {
    let mut step = step.to_owned();
    let all: String = results
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(task) = task_in(&all) {
        step = step.replace("$task", task);
    }
    if let Some(child) = child_in(&all) {
        step = step.replace("$child", child);
    }
    if let Some(from) = from {
        step = step.replace("$from", from);
    }
    step
}

/// The text of the last message, and its first tool result if it has one.
fn last_said(request: &Value) -> (String, Option<(String, bool)>) {
    let Some(last) = request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
    else {
        return (String::new(), None);
    };
    let mut text = String::new();
    let mut result = None;
    if let Some(said) = last.get("content").and_then(Value::as_str) {
        text.push_str(said);
    }
    for block in last
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_result") if result.is_none() => {
                let content = block
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                result = Some((content, error));
            }
            Some(_) | None => {}
        }
    }
    (text, result)
}

/// The tools a scripted `spawn` offers its child.
const SPAWNED_TOOLS: [&str; 6] = ["read", "bash", "send", "wake", "tasks", "cancel"];

/// What an envelope says, without the header the history gives one from
/// an agent (`[from ADDR] `) or from v (`[v] `): an agent's message is
/// scripted like the user's.
fn body(said: &str) -> &str {
    if said.starts_with("[from ") || said.starts_with("[v] ") {
        said.split_once("] ").map_or(said, |(_, body)| body)
    } else {
        said
    }
}

/// One scripted tool call, if `command` is one.
fn tool_call(command: &str) -> Option<(&'static str, Value)> {
    let command = command.trim();
    if let Some(path) = command.strip_prefix("read ") {
        return Some(("read", json!({"path": path.trim()})));
    }
    if let Some(rest) = command.strip_prefix("edit ") {
        let mut fields = rest.split_whitespace();
        let (path, old, new) = (fields.next()?, fields.next()?, fields.next()?);
        return Some(("edit", json!({"path": path, "old": old, "new": new})));
    }
    if let Some(script) = command.strip_prefix("bash ") {
        return Some(("bash", json!({"command": script})));
    }
    if let Some(script) = command.strip_prefix("bg ") {
        return Some(("bash", json!({"command": script, "background": true})));
    }
    if let Some(after) = command.strip_prefix("wake ") {
        return Some((
            "wake",
            json!({"after_ms": after.trim().parse::<u64>().ok()?}),
        ));
    }
    if command == "tasks" {
        return Some(("tasks", json!({})));
    }
    if let Some(task) = command.strip_prefix("cancel ") {
        return Some(("cancel", json!({"task": task.trim().parse::<u64>().ok()?})));
    }
    if let Some(rest) = command.strip_prefix("spawn ") {
        let (name, prompt) = rest.trim().split_once(' ')?;
        return Some((
            "spawn",
            json!({"name": name, "tools": SPAWNED_TOOLS, "prompt": prompt}),
        ));
    }
    if let Some(rest) = command.strip_prefix("send ") {
        let (to, text) = rest.trim().split_once(' ')?;
        // A placeholder no result filled names no one: no call.
        if to.starts_with('$') {
            return None;
        }
        return Some(("send", json!({"to": to, "text": text})));
    }
    if let Some(raw) = command.strip_prefix("call ") {
        let (name, input) = raw.split_once(' ').unwrap_or((raw, "{}"));
        let name: &'static str = match name {
            "read" => "read",
            "edit" => "edit",
            "bash" => "bash",
            "spawn" => "spawn",
            "send" => "send",
            "wake" => "wake",
            "tasks" => "tasks",
            "cancel" => "cancel",
            _ => "unknown_tool",
        };
        let input = serde_json::from_str(input).unwrap_or(Value::String(input.to_owned()));
        return Some((name, input));
    }
    None
}

/// `said` as a script's first step: `reply` and `ping` made sends to its
/// sender.
fn first_step(said: &str, from: Option<&str>) -> String {
    let said = body(said);
    let step = said.split(" then ").next().unwrap_or(said);
    let step = if step.trim() == "ping" {
        "reply ping"
    } else {
        step
    };
    match (step.strip_prefix("reply "), from) {
        (Some(text), Some(from)) => format!("send {from} {text}"),
        (Some(_) | None, _) => step.to_owned(),
    }
}

/// The scripted reply to a Messages request body.
#[must_use]
pub fn reply(request: &Value) -> Reply {
    let (said, result) = last_said(request);
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let turn = messages.len();
    let quick = Millis::new(0);
    let (script, step) = script(messages);
    let from = sender(&script).map(str::to_owned);
    // A later step of a script runs its next part, its placeholders filled
    // from the results it follows.
    if said.trim().is_empty() && step > 0 {
        let results = messages.last().map(said_in).unwrap_or_default().1;
        if let Some(next) = body(&script).split(" then ").nth(step) {
            let next = filled(next, &results, from.as_deref());
            let calls: Vec<(&'static str, Value)> =
                next.split(" ; ").filter_map(tool_call).collect();
            if !calls.is_empty() {
                return tool_calls(turn, &calls);
            }
        }
    }
    let said = first_step(&said, from.as_deref());
    // A tool result answers with its summary, unless the user also said
    // something new (history merges a new message into the results' turn).
    if let Some((content, error)) = result.filter(|_| said.trim().is_empty()) {
        let summary: String = content.chars().take(200).collect();
        let label = if error { "Error" } else { "Result" };
        let mut events = vec![message_start()];
        events.extend(text_block(0, &words(&format!("{label}: {summary}"))));
        events.extend(message_end("end_turn"));
        return Reply::Events {
            events,
            pace: quick,
        };
    }
    if said.starts_with("overloaded") {
        return Reply::Status {
            status: 529,
            body: overloaded_error(),
        };
    }
    if said.starts_with("error") {
        return Reply::Status {
            status: 400,
            body: json!({"type": "error", "error": {"type": "invalid_request_error", "message": "scripted"}}),
        };
    }
    if said.starts_with("fail") {
        let mut events = vec![message_start()];
        let mut partial = text_block(0, &["Partial".to_owned()]);
        partial.pop();
        events.extend(partial);
        events.push(overloaded_error());
        return Reply::Events {
            events,
            pace: quick,
        };
    }
    if said.starts_with("slow") {
        let mut events = vec![message_start()];
        events.extend(text_block(0, &vec![".".to_owned(); 40]));
        events.extend(message_end("end_turn"));
        return Reply::Events {
            events,
            pace: Millis::new(50),
        };
    }
    let calls: Vec<(&'static str, Value)> = said.split(" ; ").filter_map(tool_call).collect();
    if !calls.is_empty() {
        return tool_calls(turn, &calls);
    }
    let mut events = vec![message_start()];
    events.extend(text_block(0, &words(&format!("Echo: {said}"))));
    events.extend(message_end("end_turn"));
    Reply::Events {
        events,
        pace: quick,
    }
}

/// A response proposing `calls`, their ids made from `turn`.
fn tool_calls(turn: usize, calls: &[(&'static str, Value)]) -> Reply {
    let mut events = vec![message_start()];
    events.extend(text_block(0, &["On it.".to_owned()]));
    for (offset, (name, input)) in calls.iter().enumerate() {
        let index = offset.saturating_add(1);
        let id = format!("toolu_{turn}_{offset}");
        events.extend(tool_block(index, &id, name, input));
    }
    events.extend(message_end("tool_use"));
    Reply::Events {
        events,
        pace: Millis::new(0),
    }
}

fn sse_of(event: &Value) -> SseEvent {
    let name = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_owned();
    SseEvent::new(name, event.to_string()).expect("scripted events are valid SSE")
}

#[derive(Clone)]
struct FakeHost {
    clock: Arc<dyn Clock>,
    tasks: Tasks,
}

impl fmt::Debug for FakeHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FakeHost").finish_non_exhaustive()
    }
}

/// The largest request the host takes: Anthropic's own bound on a Messages
/// request.
const MAX_REQUEST_BYTES: usize = 32 << 20;

/// The script as an HTTP host: `POST /v1/messages`, streaming SSE.
pub fn router(clock: Arc<dyn Clock>, tasks: Tasks) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(FakeHost { clock, tasks })
}

async fn messages(State(host): State<FakeHost>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match reply(&request) {
        Reply::Status { status, body } => Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("static headers are valid"),
        Reply::Events { events, pace } => {
            let (sender, receiver) = mpsc::channel::<Bytes>(4);
            let clock = Arc::clone(&host.clock);
            host.tasks.spawn(async move {
                for event in events {
                    if pace.get() > 0 {
                        clock.sleep(pace).await;
                    }
                    let text = sse_of(&event).encode();
                    if sender.send(Bytes::from(text)).await.is_err() {
                        return;
                    }
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(Chunks { receiver }))
                .expect("static headers are valid")
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
    use serde_json::json;

    use super::{Reply, reply};

    /// The tool calls `reply` proposes: each one's name and input.
    fn proposed(reply: &Reply) -> Vec<(String, serde_json::Value)> {
        let Reply::Events { events, .. } = reply else {
            return Vec::new();
        };
        let mut calls: Vec<(String, String)> = Vec::new();
        for event in events {
            if let Some(name) = event
                .pointer("/content_block/name")
                .and_then(|name| name.as_str())
            {
                calls.push((name.to_owned(), String::new()));
            }
            if let (Some(partial), Some(last)) = (
                event
                    .pointer("/delta/partial_json")
                    .and_then(|partial| partial.as_str()),
                calls.last_mut(),
            ) {
                last.1.push_str(partial);
            }
        }
        calls
            .into_iter()
            .map(|(name, input)| (name, serde_json::from_str(&input).unwrap_or_default()))
            .collect()
    }

    #[test]
    fn a_script_runs_its_steps_with_their_placeholders_filled() {
        let first =
            json!({"messages": [{"role": "user", "content": "bg sleep 5 then cancel $task"}]});
        assert_eq!(
            proposed(&reply(&first)),
            [(
                "bash".to_owned(),
                json!({"command": "sleep 5", "background": true})
            )]
        );
        let second = json!({"messages": [
            {"role": "user", "content": "bg sleep 5 then cancel $task"},
            {"role": "assistant", "content": [{"type": "text", "text": "On it."}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1_0",
                "content": "Started task 12: it runs on, and you are told when it ends."}]},
        ]});
        assert_eq!(
            proposed(&reply(&second)),
            [("cancel".to_owned(), json!({"task": 12u64}))],
            "the next step, its task filled from the result"
        );
        let done = json!({"messages": [
            {"role": "user", "content": "bg sleep 5 then cancel $task"},
            {"role": "assistant", "content": [{"type": "text", "text": "On it."}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1_0", "content": "x"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "On it."}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_3_0", "content": "cancelled"}]},
        ]});
        assert!(proposed(&reply(&done)).is_empty(), "the script is done");
    }

    #[test]
    fn reply_and_ping_answer_the_sender() {
        let from = "0199c82c-c000-7157-b49d-191266aae0f8";
        let request = |text: String| json!({"messages": [{"role": "user", "content": text}]});
        assert_eq!(
            proposed(&reply(&request(format!(
                "[from {from}; reply to {from}] reply done"
            )))),
            [("send".to_owned(), json!({"to": from, "text": "done"}))]
        );
        assert_eq!(
            proposed(&reply(&request(format!("[from {from}] ping")))),
            [("send".to_owned(), json!({"to": from, "text": "ping"}))],
            "a ping is answered with a ping"
        );
        assert!(
            proposed(&reply(&request("send $child hi".to_owned()))).is_empty(),
            "an unfilled placeholder names no one"
        );
    }

    #[test]
    fn script_is_a_function_of_the_request() {
        let request = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "read a.txt ; read b.txt"}]}]});
        assert_eq!(
            reply(&request),
            reply(&request),
            "retries see the same reply"
        );
        let events = match reply(&request) {
            Reply::Events { events, .. } => events,
            Reply::Status { .. } => Vec::new(),
        };
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|event| {
                event
                    .pointer("/content_block/id")
                    .and_then(|id| id.as_str())
            })
            .collect();
        assert_eq!(ids, vec!["toolu_1_0", "toolu_1_1"], "deterministic ids");
        assert!(matches!(
            reply(&json!({"messages": [{"role": "user", "content": "overloaded"}]})),
            Reply::Status { status: 529, .. }
        ));
    }
}
