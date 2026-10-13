//! `update` driven directly, and views rendered with a test backend.

use core::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use serde_json::Value;
use verified_domain::id::{CallId, MessageId, SessionId, TurnId, UnixMillis};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{CreateSessionReply, Outcome, Rejection, SendMessageReply, Verdict};
use verified_wire::stream::{Control, Cursor, Freshness, Offset};
use verified_wire::transcript::TranscriptTurn;

use super::*;
use crate::terminal::{Keyboard, TestScreen};

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!(
                "../../../verified-wire/tests/fixtures/sessions/",
                $name,
                ".jsonl"
            )),
            include_str!(concat!(
                "../../../verified-wire/tests/fixtures/sessions/",
                $name,
                ".turns.json"
            )),
        )
    };
}

/// The wire crate's golden streams and the turns the AI SDK folds them into.
const FIXTURES: [(&str, &str, &str); 8] = [
    fixture!("cancelled"),
    fixture!("conversation"),
    fixture!("failed"),
    fixture!("in-progress"),
    fixture!("metadata"),
    fixture!("simple"),
    fixture!("steps"),
    fixture!("tools"),
];

fn fixture(name: &str) -> Vec<UiChunk> {
    let (_, stream, _) = FIXTURES
        .iter()
        .find(|(fixture, _, _)| *fixture == name)
        .expect("a fixture");
    chunks(stream)
}

fn chunks(stream: &str) -> Vec<UiChunk> {
    stream
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("a chunk"))
        .collect()
}

fn session() -> SessionId {
    SessionId::parse("01a10400-0000-7000-8000-0000000000f1").expect("a session id")
}

fn stamp(byte: u8) -> Stamp {
    Stamp {
        at: UnixMillis::new(1_760_000_000_000).expect("in range"),
        entropy: [byte; 10],
    }
}

fn message_id(byte: u8) -> MessageId {
    let stamp = stamp(byte);
    MessageId::from_parts(stamp.at, &stamp.entropy)
}

/// The command that sends `text` as the message minted from `stamp(byte)`.
fn sent(byte: u8, text: &str) -> Cmd {
    Cmd::Send(Command::SendMessage {
        session: session(),
        body: SendMessage {
            message_id: message_id(byte),
            text: MessageText::parse(text.to_owned(), &Limits::DEFAULT.wire).expect("text"),
        },
    })
}

fn opened() -> App {
    let (app, cmd) = App::init(Flags {
        start: Start::Open(session()),
        limits: Limits::DEFAULT,
    });
    assert!(
        matches!(cmd, Cmd::Follow { .. }),
        "opening follows the stream"
    );
    app
}

fn control(next: u64) -> Control {
    Control {
        stream_next_offset: Offset::at(next).expect("in range"),
        stream_cursor: Cursor::parse("7").expect("a cursor"),
        up_to_date: Freshness::UpToDate,
    }
}

fn stream(app: &mut App, update: StreamUpdate) -> Cmd {
    let follow = app.follow;
    app.update(Msg::Feedback(Feedback::Stream { follow, update }))
}

/// Delivers `chunks` as one read ending at `next`.
fn read(app: &mut App, chunks: Vec<UiChunk>, next: u64) -> Cmd {
    let _connected = stream(app, StreamUpdate::Connected);
    let from = app.position().offset;
    stream(
        app,
        StreamUpdate::Read {
            from,
            chunks,
            control: control(next),
        },
    )
}

fn answer(app: &mut App, answer: Answer) -> Cmd {
    app.update(Msg::Feedback(Feedback::Answered(answer)))
}

fn type_text(app: &mut App, text: &str) {
    let _cmd = app.update(Msg::Paste(text.to_owned()));
}

fn key(app: &App, code: KeyCode, modifiers: KeyModifiers) -> Option<Msg> {
    app.on_event(Keyboard::key(code, modifiers), stamp(1))
}

fn turn_id(text: &str) -> TurnId {
    TurnId::parse(text).expect("a turn id")
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut screen = TestScreen::new(width, height);
    crate::seam::Screen::draw(&mut screen, &mut |frame| app.view(frame)).expect("drawn");
    screen.text()
}

#[test]
fn a_new_session_is_created_before_it_is_followed() {
    let (mut app, cmd) = App::init(Flags {
        start: Start::New(session()),
        limits: Limits::DEFAULT,
    });
    assert_eq!(
        cmd,
        Cmd::Send(Command::CreateSession(CreateSession {
            session_id: session()
        }))
    );
    type_text(&mut app, "hi");
    assert_eq!(app.update(Msg::Submit(stamp(1))), Cmd::None);
    assert_eq!(app.draft(), "hi", "the draft waits for the session");

    let cmd = answer(
        &mut app,
        Answer::SessionCreated {
            session: session(),
            result: Ok(CreateSessionReply {
                outcome: Outcome::Exists,
            }),
        },
    );
    assert_eq!(
        cmd,
        Cmd::Follow {
            follow: FollowId(1),
            session: session(),
            from: START,
        }
    );
}

#[test]
fn a_session_that_cannot_be_created_is_retried_with_the_same_id() {
    let (mut app, _cmd) = App::init(Flags {
        start: Start::New(session()),
        limits: Limits::DEFAULT,
    });
    let _cmd = answer(
        &mut app,
        Answer::SessionCreated {
            session: session(),
            result: Err(ClientError::ClosedEarly),
        },
    );
    assert!(app.notice().is_some_and(|notice| notice.contains("^r")));
    assert_eq!(
        app.update(Msg::Reconnect),
        Cmd::Send(Command::CreateSession(CreateSession {
            session_id: session()
        }))
    );
}

#[test]
fn keys_map_to_messages() {
    let app = opened();
    assert!(matches!(
        key(&app, KeyCode::Enter, KeyModifiers::NONE),
        Some(Msg::Submit(_))
    ));
    assert!(matches!(
        key(&app, KeyCode::Enter, KeyModifiers::SHIFT),
        Some(Msg::Newline)
    ));
    assert!(matches!(
        key(&app, KeyCode::Esc, KeyModifiers::NONE),
        Some(Msg::CancelTurn)
    ));
    assert!(matches!(
        key(&app, KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Msg::Quit)
    ));
    assert!(matches!(
        key(&app, KeyCode::Char('r'), KeyModifiers::CONTROL),
        Some(Msg::Reconnect)
    ));
    assert!(matches!(
        key(&app, KeyCode::Char('y'), KeyModifiers::NONE),
        Some(Msg::Type('y'))
    ));
    assert!(matches!(
        app.on_event(Keyboard::paste("x".to_owned()), stamp(1)),
        Some(Msg::Paste(_))
    ));
}

#[test]
fn sending_mints_the_message_id_from_the_stamp() {
    let mut app = opened();
    type_text(&mut app, "  write me a haiku  ");
    let cmd = app.update(Msg::Submit(stamp(9)));
    assert_eq!(cmd, sent(9, "write me a haiku"));
    assert_eq!(app.draft(), "");
    assert!(!app.settled());

    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(9),
            result: Ok(SendMessageReply {
                turn_id: turn_id("01a10400-0000-7000-8000-000000000001"),
                outcome: Outcome::Created,
            }),
        },
    );
    assert!(app.settled());
}

#[test]
fn one_turn_at_a_time() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    assert!(app.transcript().running().is_some());
    type_text(&mut app, "second");
    assert_eq!(app.update(Msg::Submit(stamp(2))), Cmd::None);
    assert_eq!(app.draft(), "second");
    assert!(app.notice().is_some_and(|notice| notice.contains("esc")));
}

#[test]
fn a_failed_delivery_is_resent_with_the_same_id() {
    let mut app = opened();
    type_text(&mut app, "hello");
    let _cmd = app.update(Msg::Submit(stamp(3)));
    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(3),
            result: Err(ClientError::ClosedEarly),
        },
    );
    assert!(!app.settled());
    // A different stamp: the retry must still reuse the first id.
    let cmd = app.update(Msg::Submit(stamp(4)));
    assert_eq!(cmd, sent(3, "hello"));
}

#[test]
fn a_rejected_message_goes_back_to_the_draft() {
    let mut app = opened();
    type_text(&mut app, "hello");
    let _cmd = app.update(Msg::Submit(stamp(3)));
    let rejection = Rejection::SessionBusy {
        session_id: session(),
        turn_id: turn_id("01a10400-0000-7000-8000-000000000001"),
    };
    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(3),
            result: Err(ClientError::Rejected(rejection)),
        },
    );
    assert_eq!(app.draft(), "hello");
    assert!(app.settled());
    assert!(
        app.notice()
            .is_some_and(|notice| notice.contains("running turn"))
    );
}

#[test]
fn answers_for_other_messages_are_ignored() {
    let mut app = opened();
    type_text(&mut app, "hello");
    let _cmd = app.update(Msg::Submit(stamp(3)));
    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(4),
            result: Err(ClientError::ClosedEarly),
        },
    );
    assert!(
        app.pending
            .as_ref()
            .is_some_and(|pending| pending.delivery == Delivery::Sending)
    );
}

#[test]
fn the_stream_drives_the_conversation() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    assert!(app.caught_up());
    assert_eq!(app.position().offset, Offset::at(6).expect("in range"));
    assert_eq!(
        app.position().cursor,
        Some(Cursor::parse("7").expect("a cursor"))
    );
    assert_eq!(app.activity(), Activity::Replying);
}

#[test]
fn caught_up_is_false_after_a_reconnect_until_a_read() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("simple"), 11);
    assert!(app.caught_up());
    let _cmd = stream(
        &mut app,
        StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: Some(Duration::from_millis(5)),
        },
    );
    let _cmd = stream(&mut app, StreamUpdate::Connected);
    assert!(!app.caught_up(), "reconnected, but nothing read yet");
    let from = app.position().offset;
    let _cmd = stream(
        &mut app,
        StreamUpdate::Read {
            from,
            chunks: Vec::new(),
            control: control(11),
        },
    );
    assert!(app.caught_up(), "the first read says it is up to date");
}

#[test]
fn updates_from_a_replaced_follower_are_dropped() {
    let mut app = opened();
    let stale = app.follow;
    let _cmd = stream(
        &mut app,
        StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: None,
        },
    );
    let _cmd = app.update(Msg::Reconnect);
    let _cmd = app.update(Msg::Feedback(Feedback::Stream {
        follow: stale,
        update: StreamUpdate::Read {
            from: Offset::BEGINNING,
            chunks: fixture("simple"),
            control: control(11),
        },
    }));
    assert!(app.transcript().turns().is_empty());
    assert_eq!(app.position(), START);
}

#[test]
fn escape_cancels_the_running_turn_once() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    let running = app
        .transcript()
        .running()
        .map(TranscriptTurn::id)
        .expect("a running turn");
    assert_eq!(
        app.update(Msg::CancelTurn),
        Cmd::Send(Command::Cancel {
            session: session(),
            turn: running,
        })
    );
    assert_eq!(app.update(Msg::CancelTurn), Cmd::None, "already cancelling");
}

#[test]
fn only_the_answer_for_the_turn_being_cancelled_settles_it() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    let running = app
        .transcript()
        .running()
        .map(TranscriptTurn::id)
        .expect("a running turn");
    let _cmd = app.update(Msg::CancelTurn);
    let other = turn_id("01a10400-0000-7000-8000-0000000000ff");
    let _cmd = answer(
        &mut app,
        Answer::Cancelled {
            turn_id: other,
            result: Err(ClientError::ClosedEarly),
        },
    );
    assert!(!app.settled(), "an answer about another turn");
    assert_eq!(app.notice(), None);
    let rejection = Rejection::TurnNotRunning { turn_id: running };
    let _cmd = answer(
        &mut app,
        Answer::Cancelled {
            turn_id: running,
            result: Err(ClientError::Rejected(rejection)),
        },
    );
    assert!(app.settled());
    assert!(
        app.notice()
            .is_some_and(|notice| notice.contains("is not running"))
    );
}

#[test]
#[should_panic(expected = "delivered a read from 0000000000000007")]
fn a_read_that_skips_past_the_fold_is_a_defect() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    let _cmd = stream(
        &mut app,
        StreamUpdate::Read {
            from: Offset::at(7).expect("in range"),
            chunks: Vec::new(),
            control: control(8),
        },
    );
}

#[test]
#[should_panic(expected = "delivered a read from 0000000000000000")]
fn a_read_folded_twice_is_a_defect() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    let _cmd = stream(
        &mut app,
        StreamUpdate::Read {
            from: Offset::BEGINNING,
            chunks: fixture("in-progress"),
            control: control(6),
        },
    );
}

#[test]
fn escape_without_a_running_turn_does_nothing() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("simple"), 11);
    assert_eq!(app.update(Msg::CancelTurn), Cmd::None);
}

#[test]
fn the_approval_prompt_takes_y_and_n() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("tools"), 71);
    let tool = app.awaiting_approval().expect("toolu_8 awaits approval");
    assert_eq!(tool.tool_call_id.as_str(), "toolu_8");

    assert!(matches!(
        key(&app, KeyCode::Char('y'), KeyModifiers::NONE),
        Some(Msg::Decide(Verdict::Grant))
    ));
    assert!(matches!(
        key(&app, KeyCode::Char('n'), KeyModifiers::NONE),
        Some(Msg::Decide(Verdict::Deny))
    ));
    assert!(key(&app, KeyCode::Char('x'), KeyModifiers::NONE).is_none());

    let call_id = CallId::parse("toolu_8").expect("a call id");
    assert_eq!(
        app.update(Msg::Decide(Verdict::Deny)),
        Cmd::Send(Command::Approve {
            session: session(),
            body: Approve {
                call_id: call_id.clone(),
                verdict: Verdict::Deny
            },
        })
    );
    assert_eq!(
        app.update(Msg::Decide(Verdict::Grant)),
        Cmd::None,
        "one answer at a time"
    );
    let _cmd = answer(
        &mut app,
        Answer::Approved {
            call_id,
            result: Ok(verified_wire::command::ApproveReply {
                outcome: Outcome::Created,
            }),
        },
    );
    assert!(app.settled());
}

#[test]
fn a_chunk_that_does_not_fold_breaks_the_stream_until_reloaded() {
    let mut app = opened();
    let mut broken = fixture("simple");
    broken.insert(0, broken.get(5).cloned().expect("a chunk"));
    let cmd = read(&mut app, broken, 12);
    assert_eq!(cmd, Cmd::StopFollowing);
    assert!(matches!(app.connection(), Connection::Broken(_)));
    // What the stopped follower had already queued is stale.
    let _cmd = app.update(Msg::Feedback(Feedback::Stream {
        follow: FollowId(1),
        update: StreamUpdate::Read {
            from: Offset::at(12).expect("in range"),
            chunks: Vec::new(),
            control: control(12),
        },
    }));
    let _cmd = app.update(Msg::Feedback(Feedback::Stream {
        follow: FollowId(1),
        update: StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: Some(Duration::from_millis(1)),
        },
    }));
    assert!(matches!(app.connection(), Connection::Broken(_)));
    assert_eq!(
        app.position(),
        START,
        "nothing past the broken read is acknowledged"
    );

    let cmd = app.update(Msg::Reconnect);
    assert_eq!(
        cmd,
        Cmd::Follow {
            follow: FollowId(3),
            session: session(),
            from: START,
        }
    );
    assert!(app.transcript().turns().is_empty(), "reloads from scratch");
}

#[test]
fn reconnecting_resumes_from_the_last_offset() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("simple"), 11);
    let _cmd = stream(
        &mut app,
        StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: None,
        },
    );
    assert!(matches!(app.connection(), Connection::Down(_)));
    assert!(!app.caught_up());
    assert_eq!(
        app.update(Msg::Reconnect),
        Cmd::Follow {
            follow: FollowId(2),
            session: session(),
            from: Position {
                offset: Offset::at(11).expect("in range"),
                cursor: Some(Cursor::parse("7").expect("a cursor")),
            },
        }
    );
    assert_eq!(app.transcript().turns().len(), 1, "the transcript is kept");
}

#[test]
fn retrying_shows_when() {
    let mut app = opened();
    let _cmd = stream(
        &mut app,
        StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: Some(Duration::from_millis(500)),
        },
    );
    assert!(matches!(app.connection(), Connection::Retrying(reason) if reason.contains("500ms")));
}

#[test]
fn the_draft_is_bounded() {
    let mut limits = Limits::DEFAULT;
    limits.wire.max_message_text_bytes = 4;
    let (mut app, _cmd) = App::init(Flags {
        start: Start::Open(session()),
        limits,
    });
    type_text(&mut app, "abcd");
    let _cmd = app.update(Msg::Type('e'));
    assert_eq!(app.draft(), "abcd");
    assert!(app.notice().is_some());
    let _cmd = app.update(Msg::Backspace);
    assert_eq!(app.draft(), "abc");
}

#[test]
fn paging_up_stops_following_and_paging_back_down_resumes() {
    let mut app = opened();
    app.viewport = Viewport {
        lines: 100,
        height: 20,
    };
    let _cmd = app.update(Msg::Scroll(Scroll::PageUp));
    assert_eq!(app.scroll, Some(62));
    let _cmd = app.update(Msg::Scroll(Scroll::PageDown));
    assert_eq!(app.scroll, None);
}

/// Folding each golden stream through the app gives the AI SDK's turns.
#[test]
fn the_app_folds_every_golden_stream_like_the_ai_sdk() {
    for (name, stream, turns) in FIXTURES {
        let mut app = opened();
        let chunks = chunks(stream);
        let next = u64::try_from(chunks.len()).expect("fits");
        let _cmd = read(&mut app, chunks, next);
        let folded = serde_json::to_value(app.transcript().turns()).expect("serializes");
        let golden: Value = serde_json::from_str(turns).expect("golden JSON");
        assert_eq!(folded, golden, "{name}");
    }
}

/// Folding one chunk per read gives the same transcript as one read.
#[test]
fn reads_of_any_size_fold_the_same() {
    for (name, stream, _) in FIXTURES {
        let whole = {
            let mut app = opened();
            let chunks = chunks(stream);
            let _cmd = read(&mut app, chunks, 1);
            serde_json::to_value(app.transcript().turns()).expect("serializes")
        };
        let mut app = opened();
        for (seq, chunk) in chunks(stream).into_iter().enumerate() {
            let next = u64::try_from(seq).expect("fits").saturating_add(1);
            let _cmd = read(&mut app, vec![chunk], next);
        }
        let piecewise = serde_json::to_value(app.transcript().turns()).expect("serializes");
        assert_eq!(whole, piecewise, "{name}");
    }
}

#[test]
fn renders_every_golden_stream() {
    for (name, stream, _) in FIXTURES {
        let mut app = opened();
        let chunks = chunks(stream);
        let next = u64::try_from(chunks.len()).expect("fits");
        let _cmd = read(&mut app, chunks, next);
        insta::assert_snapshot!(name, screen(&mut app, 80, 30));
    }
}

#[test]
fn renders_a_new_session_being_created() {
    let (mut app, _cmd) = App::init(Flags {
        start: Start::New(session()),
        limits: Limits::DEFAULT,
    });
    type_text(&mut app, "first line\nsecond line");
    insta::assert_snapshot!(screen(&mut app, 80, 10));
}

#[test]
fn renders_a_dropped_connection_with_a_failed_message() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("simple"), 11);
    type_text(&mut app, "are you there?");
    let _cmd = app.update(Msg::Submit(stamp(5)));
    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(5),
            result: Err(ClientError::ClosedEarly),
        },
    );
    let _cmd = stream(
        &mut app,
        StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: None,
        },
    );
    insta::assert_snapshot!(screen(&mut app, 100, 14));
}

#[test]
fn capacity_refusals_are_retried_and_the_rest_are_final() {
    use crate::client::Recovery;
    let full = ClientError::Rejected(Rejection::TooManyFollowers {
        session_id: session(),
        limit: 4,
    });
    assert_eq!(full.recovery(), Recovery::Retry);
    let sessions = ClientError::Rejected(Rejection::TooManySessions { limit: 8 });
    assert_eq!(sessions.recovery(), Recovery::Retry);
    let session_full = Rejection::SessionFull {
        session_id: session(),
        reason: "too many turns".to_owned(),
    };
    assert_eq!(
        ClientError::Rejected(session_full.clone()).recovery(),
        Recovery::GiveUp
    );

    let mut app = opened();
    type_text(&mut app, "hello");
    let _cmd = app.update(Msg::Submit(stamp(3)));
    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(3),
            result: Err(ClientError::Rejected(session_full)),
        },
    );
    assert_eq!(app.draft(), "hello");
    assert!(
        app.notice()
            .is_some_and(|notice| notice.contains("is full") && notice.contains("new session"))
    );
}
