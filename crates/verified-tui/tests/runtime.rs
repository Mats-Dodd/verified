//! The whole client (runtime, HTTP client, follower, app) against a scripted
//! daemon over in-memory pipes: create the session, catch up on the stream,
//! reconnect from the last offset after the daemon closes it, send a message
//! with a stamp-minted id, and quit.

use core::time::Duration;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyModifiers};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use verified_domain::id::{IdEntropy, MessageId, UnixMillis};
use verified_tui::app::Start;
use verified_tui::seam::{Clock, Dialer, Entropy, Stream};
use verified_tui::tea::Seams;
use verified_tui::terminal::{Keyboard, TestScreen, scripted};
use verified_tui::view::Connection;
use verified_tui::{Limits, new_session};
use verified_wire::WireLimits;
use verified_wire::chunk::UiChunk;
use verified_wire::command::SendMessage;
use verified_wire::stream::{Control, Cursor, Freshness, Offset, StreamEvent};

/// Runs a test's in-memory server on the test's runtime: test scaffolding,
/// not a task of the code under test.
#[expect(
    clippy::disallowed_methods,
    reason = "the test's own fake server; the TUI under test starts no task itself"
)]
fn serve_in_background<F>(server: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    drop(tokio::spawn(server));
}

const SIMPLE: &str = include_str!("../../verified-wire/tests/fixtures/sessions/simple.jsonl");
const SIMPLE_TURNS: &str =
    include_str!("../../verified-wire/tests/fixtures/sessions/simple.turns.json");
const NOW: u64 = 1_760_000_000_000;

#[derive(Debug)]
struct FixedClock;

#[async_trait]
impl Clock for FixedClock {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(NOW).expect("in range")
    }

    /// Retry backoffs pass at once; deadlines (the exchange and stream-idle
    /// timeouts, longer than any backoff) never fire, because the scripted
    /// daemon always answers.
    async fn sleep(&self, duration: Duration) {
        if duration >= Limits::DEFAULT.exchange_timeout {
            std::future::pending::<()>().await;
        }
        tokio::task::yield_now().await;
    }
}

/// Entropy `[n; 10]` for the n-th draw, counting from one.
#[derive(Debug, Default)]
struct Counting(u8);

impl Entropy for Counting {
    fn id_entropy(&mut self) -> IdEntropy {
        self.0 = self.0.checked_add(1).expect("fewer than 256 draws");
        [self.0; 10]
    }
}

/// What the scripted daemon saw.
#[derive(Debug)]
enum Seen {
    Request { line: String },
    Message(Vec<u8>),
}

/// Each dial is a fresh in-memory connection served by [`serve`].
#[derive(Debug)]
struct ScriptedDaemon {
    seen: SyncSender<Seen>,
}

#[async_trait]
impl Dialer for ScriptedDaemon {
    fn authority(&self) -> &'static str {
        "daemon:7777"
    }

    async fn dial(&self) -> io::Result<Box<dyn Stream>> {
        let (client, server) = tokio::io::duplex(1 << 16);
        serve_in_background(serve(server, self.seen.clone()));
        Ok(Box::new(client))
    }
}

fn simple_chunks() -> Vec<UiChunk> {
    SIMPLE
        .lines()
        .map(|line| serde_json::from_str(line).expect("a chunk"))
        .collect()
}

fn control(next: u64) -> String {
    StreamEvent::Control(Control {
        stream_next_offset: Offset::at(next).expect("in range"),
        stream_cursor: Cursor::parse("7").expect("a cursor"),
        up_to_date: Freshness::UpToDate,
    })
    .to_sse()
    .encode()
}

async fn respond_json(mut stream: DuplexStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("written");
    stream.shutdown().await.expect("closed");
}

/// Reads one request: its request line and its body.
async fn read_request(stream: &mut DuplexStream) -> (String, Vec<u8>) {
    let mut request = Vec::new();
    let head_end = loop {
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).await.expect("read");
        assert!(read > 0, "the client closed before sending a request");
        request.extend_from_slice(buffer.get(..read).expect("in bounds"));
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end.saturating_add(4);
        }
    };
    let head = String::from_utf8(request.get(..head_end).expect("in bounds").to_vec())
        .expect("UTF-8 head");
    let line = head.lines().next().expect("a request line").to_owned();
    let length: usize = head
        .lines()
        .find_map(|header| header.strip_prefix("content-length: "))
        .map_or(0, |length| length.trim().parse().expect("a length"));
    let mut body = request.get(head_end..).expect("in bounds").to_vec();
    while body.len() < length {
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).await.expect("read");
        body.extend_from_slice(buffer.get(..read).expect("in bounds"));
    }
    (line, body)
}

/// Serves one request.
async fn serve(mut stream: DuplexStream, seen: SyncSender<Seen>) {
    let (line, body) = read_request(&mut stream).await;
    seen.send(Seen::Request { line: line.clone() })
        .expect("the test is listening");

    if line.starts_with("POST /sessions ") {
        respond_json(stream, r#"{"outcome":"created"}"#).await;
    } else if line.contains("/messages ") {
        seen.send(Seen::Message(body))
            .expect("the test is listening");
        let reply = r#"{"turnId":"01a10400-0000-7000-8000-000000000002","outcome":"created"}"#;
        respond_json(stream, reply).await;
    } else if line.contains("offset=0000000000000000") {
        // The first read: the whole stream, then the daemon closes the
        // connection, as it does periodically.
        let mut body = StreamEvent::Data(simple_chunks()).to_sse().encode();
        body.push_str(&control(11));
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        stream.write_all(head.as_bytes()).await.expect("written");
        stream.write_all(body.as_bytes()).await.expect("written");
        stream.shutdown().await.expect("closed");
    } else {
        // The resumed read: nothing new; the connection stays open.
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        stream.write_all(head.as_bytes()).await.expect("written");
        stream
            .write_all(control(11).as_bytes())
            .await
            .expect("written");
        core::future::pending::<()>().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    CatchingUp,
    Sending,
    Quitting,
}

#[tokio::test]
async fn creates_catches_up_reconnects_sends_and_quits() {
    let (seen_by_daemon, seen): (SyncSender<Seen>, Receiver<Seen>) = sync_channel(64);
    let (keyboard, input) = scripted(16);
    let mut entropy = Counting::default();
    let session = new_session(&FixedClock, &mut entropy);
    let seams = Seams {
        input: Box::new(input),
        dialer: Arc::new(ScriptedDaemon {
            seen: seen_by_daemon,
        }),
        clock: Arc::new(FixedClock),
        entropy: Box::new(entropy),
    };
    let mut screen = TestScreen::new(80, 12);
    let mut script = Script::CatchingUp;
    let mut seen_log = Vec::new();
    let mut observe = |app: &verified_tui::app::App| {
        seen_log.extend(seen.try_iter());
        // The scripted daemon never fails, so a stopped stream is a client
        // bug (e.g. resuming from the wrong offset); fail rather than hang.
        assert!(
            !matches!(
                app.connection(),
                Connection::Down(_) | Connection::Broken(_)
            ),
            "the stream stopped: {:?}",
            app.connection()
        );
        match script {
            Script::CatchingUp if app.caught_up() && app.transcript().turns().len() == 1 => {
                keyboard
                    .try_send(Keyboard::paste("hi".to_owned()))
                    .expect("queued");
                keyboard.try_send(Keyboard::enter()).expect("queued");
                script = Script::Sending;
            }
            Script::Sending
                if app.settled()
                    && seen_log.iter().any(|seen| matches!(seen, Seen::Message(_))) =>
            {
                keyboard.try_send(Keyboard::quit()).expect("queued");
                script = Script::Quitting;
            }
            Script::CatchingUp | Script::Sending | Script::Quitting => {}
        }
    };

    let app = verified_tui::run(
        Start::New(session),
        Limits::DEFAULT,
        seams,
        &mut screen,
        &mut observe,
    )
    .await
    .expect("runs");
    assert_eq!(script, Script::Quitting);
    seen_log.extend(seen.try_iter());

    // I6 in miniature: the folded transcript is the AI SDK's fold of the stream.
    let folded = serde_json::to_value(app.transcript().turns()).expect("serializes");
    let golden: Value = serde_json::from_str(SIMPLE_TURNS).expect("golden");
    assert_eq!(folded, golden);
    assert!(app.caught_up());
    assert_eq!(app.position().offset, Offset::at(11).expect("in range"));

    let lines: Vec<&str> = seen_log
        .iter()
        .filter_map(|seen| match seen {
            Seen::Request { line } => Some(line.as_str()),
            Seen::Message(_) => None,
        })
        .collect();
    assert_eq!(lines.first(), Some(&"POST /sessions HTTP/1.1"));
    let stream = format!("GET /sessions/{session}/stream?");
    assert!(lines.contains(&format!("{stream}live=sse&offset=0000000000000000 HTTP/1.1").as_str()));
    assert!(
        lines.contains(
            &format!("{stream}cursor=7&live=sse&offset=0000000000000011 HTTP/1.1").as_str()
        ),
        "resumes from the last control offset, echoing the cursor: {lines:?}"
    );

    // The session id was the first draw, the paste the second, Enter the third.
    let body = seen_log
        .iter()
        .find_map(|seen| match seen {
            Seen::Message(body) => Some(body),
            Seen::Request { .. } => None,
        })
        .expect("a message was posted");
    let message = SendMessage::decode(body, &WireLimits::DEFAULT).expect("a message body");
    assert_eq!(message.text.as_str(), "hi");
    let at = UnixMillis::new(NOW).expect("in range");
    assert_eq!(message.message_id, MessageId::from_parts(at, &[3; 10]));
    assert!(screen.text().contains("Hi there, friend."));
}

/// Refuses the first `refusals` dials, then serves like [`ScriptedDaemon`].
#[derive(Debug)]
struct FlakyDaemon {
    #[expect(
        clippy::disallowed_types,
        reason = "test-only dial counter; the client is its only writer and it is read after the run"
    )]
    dials: std::sync::atomic::AtomicU32,
    refusals: u32,
    seen: SyncSender<Seen>,
}

#[async_trait]
impl Dialer for FlakyDaemon {
    fn authority(&self) -> &'static str {
        "daemon:7777"
    }

    async fn dial(&self) -> io::Result<Box<dyn Stream>> {
        let dial = self
            .dials
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dial < self.refusals {
            return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
        }
        let (client, server) = tokio::io::duplex(1 << 16);
        serve_in_background(serve(server, self.seen.clone()));
        Ok(Box::new(client))
    }
}

fn flaky(refusals: u32) -> (Arc<FlakyDaemon>, Receiver<Seen>) {
    let (seen, receiver) = sync_channel(64);
    #[expect(
        clippy::disallowed_types,
        reason = "the FlakyDaemon dial counter, as above"
    )]
    let dials = std::sync::atomic::AtomicU32::new(0);
    let daemon = FlakyDaemon {
        dials,
        refusals,
        seen,
    };
    (Arc::new(daemon), receiver)
}

fn hello(entropy: u8) -> verified_tui::client::Command {
    let at = UnixMillis::new(NOW).expect("in range");
    verified_tui::client::Command::SendMessage {
        session: verified_domain::id::SessionId::from_parts(at, &[0; 10]),
        body: SendMessage {
            message_id: MessageId::from_parts(at, &[entropy; 10]),
            text: verified_wire::command::MessageText::parse(
                "hello".to_owned(),
                &WireLimits::DEFAULT,
            )
            .expect("text"),
        },
    }
}

#[tokio::test]
async fn commands_are_retried_with_the_same_body_until_delivered() {
    let (daemon, seen) = flaky(3);
    let client = verified_tui::client::Client::new(
        Arc::<FlakyDaemon>::clone(&daemon),
        Arc::new(FixedClock),
        Limits::DEFAULT,
    );
    let answer = client.send(hello(5)).await;
    assert!(
        matches!(
            answer,
            verified_tui::client::Answer::MessageSent { result: Ok(_), .. }
        ),
        "{answer:?}"
    );
    assert_eq!(daemon.dials.load(std::sync::atomic::Ordering::Relaxed), 4);
    let bodies: Vec<Vec<u8>> = seen
        .try_iter()
        .filter_map(|seen| match seen {
            Seen::Message(body) => Some(body),
            Seen::Request { .. } => None,
        })
        .collect();
    assert_eq!(bodies.len(), 1, "one delivery reached the daemon");
    let body = bodies.first().expect("one body");
    let at = UnixMillis::new(NOW).expect("in range");
    let message = SendMessage::decode(body, &WireLimits::DEFAULT).expect("a body");
    assert_eq!(message.message_id, MessageId::from_parts(at, &[5; 10]));
}

#[tokio::test]
async fn commands_give_up_when_the_schedule_is_spent() {
    let (daemon, _seen) = flaky(u32::MAX);
    let client = verified_tui::client::Client::new(
        Arc::<FlakyDaemon>::clone(&daemon),
        Arc::new(FixedClock),
        Limits::DEFAULT,
    );
    let answer = client.send(hello(6)).await;
    assert!(
        matches!(
            answer,
            verified_tui::client::Answer::MessageSent {
                result: Err(verified_tui::client::ClientError::Connect(_)),
                ..
            }
        ),
        "{answer:?}"
    );
    let attempts = Limits::DEFAULT.command_retry.attempts.saturating_add(1);
    assert_eq!(
        daemon.dials.load(std::sync::atomic::Ordering::Relaxed),
        attempts
    );
}

/// A stream of [`RECORDS`] records; record `seq` projects to the chunks at
/// `RECORD_STARTS[seq]..RECORD_STARTS[seq + 1]` of the simple fixture.
const RECORDS: u64 = 6;
const RECORD_STARTS: [usize; 7] = [0, 1, 3, 4, 7, 9, 11];

fn record(seq: u64) -> Vec<UiChunk> {
    let index = usize::try_from(seq).expect("small");
    let start = *RECORD_STARTS.get(index).expect("a record");
    let end = *RECORD_STARTS
        .get(index.saturating_add(1))
        .expect("a record");
    simple_chunks().get(start..end).expect("in bounds").to_vec()
}

/// How each connection of [`DroppingDaemon`] ends: after serving whole
/// records up to `cut` (exclusive), it writes `torn` bytes of record `cut`'s
/// data event and drops the connection; `None` serves to the end and stays.
const CUTS: [Option<(u64, usize)>; 3] = [
    // Drops between a data event and its control event.
    Some((3, usize::MAX)),
    // Drops in the middle of a data event.
    Some((5, 17)),
    None,
];

/// What the dropping daemon saw and did.
#[derive(Debug)]
enum Served {
    /// A stream read was requested from this offset, echoing this cursor.
    Opened { offset: Offset, cursor: bool },
    /// A record's data and control event were written whole.
    Record { seq: u64, chunks: Vec<UiChunk> },
}

/// Serves a session stream of [`RECORDS`] records from whatever offset each
/// read asks for, one record per read, dropping connections per [`CUTS`].
#[derive(Debug)]
struct DroppingDaemon {
    #[expect(
        clippy::disallowed_types,
        reason = "test-only dial counter, as in FlakyDaemon"
    )]
    dials: std::sync::atomic::AtomicUsize,
    served: SyncSender<Served>,
}

#[async_trait]
impl Dialer for DroppingDaemon {
    fn authority(&self) -> &'static str {
        "daemon:7777"
    }

    async fn dial(&self) -> io::Result<Box<dyn Stream>> {
        let dial = self
            .dials
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cut = *CUTS.get(dial).expect("no more connections than scripted");
        let (client, server) = tokio::io::duplex(1 << 16);
        serve_in_background(serve_records(server, cut, self.served.clone()));
        Ok(Box::new(client))
    }
}

async fn serve_records(
    mut stream: DuplexStream,
    cut: Option<(u64, usize)>,
    served: SyncSender<Served>,
) {
    let (line, _body) = read_request(&mut stream).await;
    let query = line
        .split_once('?')
        .and_then(|(_, rest)| rest.split_once(' '))
        .map(|(query, _)| query)
        .expect("a stream read");
    let offset = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("offset="))
        .map(|offset| Offset::parse(offset).expect("an offset"))
        .expect("an offset parameter");
    let cursor = query.split('&').any(|pair| pair.starts_with("cursor="));
    served
        .send(Served::Opened { offset, cursor })
        .expect("the test is listening");
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).await.expect("written");
    let end = cut.map_or(RECORDS, |(cut, _)| cut);
    for seq in offset.seq()..end {
        let chunks = record(seq);
        let mut event = StreamEvent::Data(chunks.clone()).to_sse().encode();
        event.push_str(&control(seq.saturating_add(1)));
        stream.write_all(event.as_bytes()).await.expect("written");
        if seq == 3 {
            // A keep-alive: a read with no chunks that does not advance.
            let keep_alive = control(seq.saturating_add(1));
            stream
                .write_all(keep_alive.as_bytes())
                .await
                .expect("written");
        }
        served
            .send(Served::Record { seq, chunks })
            .expect("the test is listening");
    }
    if let Some((cut, torn)) = cut {
        if cut < RECORDS {
            let event = StreamEvent::Data(record(cut)).to_sse().encode();
            let torn = event.get(..torn.min(event.len())).expect("ASCII-safe cut");
            stream.write_all(torn.as_bytes()).await.expect("written");
        }
        stream.shutdown().await.expect("closed");
    } else {
        // Up to date: one more empty read at the end, so the test stops
        // even when the client asked for a read past the last record.
        let at_end = control(offset.seq().max(RECORDS));
        stream.write_all(at_end.as_bytes()).await.expect("written");
        core::future::pending::<()>().await;
    }
}

#[tokio::test]
async fn a_dropped_stream_resumes_at_exactly_the_next_unseen_record() {
    let (served_by_daemon, served) = sync_channel(64);
    #[expect(clippy::disallowed_types, reason = "the DroppingDaemon dial counter")]
    let dials = std::sync::atomic::AtomicUsize::new(0);
    let daemon = Arc::new(DroppingDaemon {
        dials,
        served: served_by_daemon,
    });
    let client = verified_tui::client::Client::new(
        Arc::<DroppingDaemon>::clone(&daemon),
        Arc::new(FixedClock),
        Limits::DEFAULT,
    );
    let at = UnixMillis::new(NOW).expect("in range");
    let session = verified_domain::id::SessionId::from_parts(at, &[0; 10]);
    let start = verified_tui::client::Position {
        offset: Offset::BEGINNING,
        cursor: None,
    };
    let stop = tokio_util::sync::CancellationToken::new();
    let (inbox, mut updates) = tokio::sync::mpsc::channel(8);
    let end = Offset::at(RECORDS).expect("in range");

    let follower = async {
        client
            .follow(session, start, &inbox, |update| update, &stop)
            .await;
        drop(inbox);
    };
    let collector = async {
        let mut log = Vec::new();
        while let Some(update) = updates.recv().await {
            if let verified_tui::client::StreamUpdate::Read { control, .. } = &update
                && control.stream_next_offset >= end
            {
                stop.cancel();
            }
            log.push(update);
        }
        log
    };
    let ((), updates) = tokio::join!(follower, collector);

    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, verified_tui::client::StreamUpdate::Ended { .. })),
        "the follower gave up: {updates:#?}"
    );

    // Every read the client received, keyed by the offset it started at.
    let mut received: std::collections::BTreeMap<u64, Vec<UiChunk>> =
        std::collections::BTreeMap::new();
    let mut expected_from = Offset::BEGINNING;
    for update in &updates {
        match update {
            verified_tui::client::StreamUpdate::Read {
                from,
                chunks,
                control,
            } => {
                assert_eq!(*from, expected_from, "reads tile the stream: {updates:#?}");
                expected_from = control.stream_next_offset;
                if chunks.is_empty() {
                    assert_eq!(control.stream_next_offset, *from, "a keep-alive");
                    continue;
                }
                assert_eq!(
                    control.stream_next_offset.seq(),
                    from.seq().saturating_add(1),
                    "one record per read"
                );
                let previous = received.insert(from.seq(), chunks.clone());
                assert!(previous.is_none(), "offset {from} received twice");
            }
            verified_tui::client::StreamUpdate::Connected
            | verified_tui::client::StreamUpdate::Disconnected { .. }
            | verified_tui::client::StreamUpdate::Ended { .. } => {}
        }
    }

    // What the daemon served whole, per offset, and where each read began.
    let mut opened = Vec::new();
    let mut whole: std::collections::BTreeMap<u64, Vec<UiChunk>> =
        std::collections::BTreeMap::new();
    for served in served.try_iter() {
        match served {
            Served::Opened { offset, cursor } => opened.push((offset.seq(), cursor)),
            Served::Record { seq, chunks } => {
                let previous = whole.insert(seq, chunks);
                assert!(previous.is_none(), "the daemon served {seq} twice");
            }
        }
    }
    assert_eq!(
        opened,
        [(0, false), (3, true), (5, true)],
        "each reconnect asks for exactly the next unseen record, echoing the cursor"
    );
    assert_eq!(
        received.keys().copied().collect::<Vec<_>>(),
        (0..RECORDS).collect::<Vec<_>>(),
        "no gap"
    );
    assert_eq!(received, whole, "per offset, what the daemon served");
}

/// A daemon that never answers a dial.
#[derive(Debug)]
struct Unreachable;

#[async_trait]
impl Dialer for Unreachable {
    fn authority(&self) -> &'static str {
        "daemon:7777"
    }

    async fn dial(&self) -> io::Result<Box<dyn Stream>> {
        std::future::pending().await
    }
}

/// Ctrl-R held down while the daemon never answers: each press only
/// restarts what has stopped, so the effects in flight stay bounded and the
/// client quits cleanly.
#[tokio::test]
async fn holding_ctrl_r_while_the_daemon_is_silent_starts_nothing_new() {
    for open in [false, true] {
        let (keyboard, input) = scripted(16);
        let mut entropy = Counting::default();
        let session = new_session(&FixedClock, &mut entropy);
        let seams = Seams {
            input: Box::new(input),
            dialer: Arc::new(Unreachable),
            clock: Arc::new(FixedClock),
            entropy: Box::new(entropy),
        };
        for _ in 0..7u8 {
            keyboard
                .try_send(Keyboard::key(KeyCode::Char('r'), KeyModifiers::CONTROL))
                .expect("room for the key");
        }
        keyboard
            .try_send(Keyboard::quit())
            .expect("room for the key");
        let start = if open {
            Start::Open(session)
        } else {
            Start::New(session)
        };
        let mut screen = TestScreen::new(80, 12);
        let app = verified_tui::run(start, Limits::DEFAULT, seams, &mut screen, &mut |_| {})
            .await
            .expect("the client quits");
        assert_eq!(app.session(), session, "{start:?}");
    }
}
