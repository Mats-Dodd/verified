//! `update` driven directly, and views rendered with a test backend.

use crossterm::event::{KeyCode, KeyModifiers};
use serde_json::Value;
use verified_domain::Millis;
use verified_domain::id::{ActorId, CallId, MessageId, TurnId, UnixMillis};
use verified_wire::chunk::UiChunk;
use verified_wire::command::{
    ActorList, ActorState, ActorSummary, CreateActorReply, Outcome, Rejection, SendMessageReply,
    Verdict,
};
use verified_wire::stream::{Control, Freshness, Offset};
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
    fixture!("simple"),
    fixture!("steering"),
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

fn session() -> ActorId {
    ActorId::parse("01a10400-0000-7000-8000-0000000000f1").expect("a session id")
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
        actor: session(),
        body: SendMessage {
            message_id: message_id(byte),
            text: MessageText::parse(text.to_owned(), &Limits::DEFAULT.wire).expect("text"),
        },
    })
}

fn opened() -> App {
    let (app, cmd) = App::init(Start::Open(session()), Limits::DEFAULT);
    assert_eq!(
        cmd,
        Cmd::Batch(vec![
            Cmd::Follow {
                follow: FollowId(1),
                actor: session(),
                from: Offset::BEGINNING,
            },
            Cmd::Batch(vec![
                Cmd::Send(Command::ListActors),
                Cmd::Tick(Limits::DEFAULT.refresh)
            ]),
        ]),
        "opening follows the stream, lists the actors and ticks"
    );
    app
}

fn control(next: u64) -> Control {
    Control {
        stream_next_offset: Offset::at(next).expect("in range"),
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
    let from = app.offset();
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

const fn turn_id(seq: u64) -> TurnId {
    TurnId(seq)
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut screen = TestScreen::new(width, height);
    crate::seam::Screen::draw(&mut screen, &mut |frame| app.view(frame)).expect("drawn");
    screen.text()
}

#[test]
fn a_new_session_is_created_before_it_is_followed() {
    let (mut app, cmd) = App::init(Start::New(session()), Limits::DEFAULT);
    assert_eq!(
        cmd,
        Cmd::Batch(vec![
            Cmd::Send(Command::CreateActor(CreateActor {
                actor_id: session()
            })),
            Cmd::Batch(vec![Cmd::None, Cmd::Tick(Limits::DEFAULT.refresh)]),
        ]),
        "nothing is listed before the actor exists"
    );
    type_text(&mut app, "hi");
    assert_eq!(app.update(Msg::Submit(stamp(1))), Cmd::None);
    assert_eq!(app.draft(), "hi", "the draft waits for the actor");

    let cmd = answer(
        &mut app,
        Answer::ActorCreated {
            actor: session(),
            result: Ok(CreateActorReply {
                outcome: Outcome::Exists,
            }),
        },
    );
    assert_eq!(
        cmd,
        Cmd::Batch(vec![
            Cmd::Follow {
                follow: FollowId(1),
                actor: session(),
                from: Offset::BEGINNING,
            },
            Cmd::Send(Command::ListActors),
        ])
    );
}

#[test]
fn a_session_that_cannot_be_created_is_retried_with_the_same_id() {
    let (mut app, _cmd) = App::init(Start::New(session()), Limits::DEFAULT);
    let _cmd = answer(
        &mut app,
        Answer::ActorCreated {
            actor: session(),
            result: Err(ClientError::ClosedEarly),
        },
    );
    assert!(app.notice().is_some_and(|notice| notice.contains("^r")));
    assert_eq!(
        app.update(Msg::Reconnect),
        Cmd::Send(Command::CreateActor(CreateActor {
            actor_id: session()
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
                turn_id: turn_id(1),
                outcome: Outcome::Created,
            }),
        },
    );
    assert!(app.settled());
}

/// Typing while a turn runs sends at once: the message steers the turn.
/// It shows as queued until a step carries it.
#[test]
fn a_message_typed_while_a_turn_runs_is_sent_to_steer_it() {
    let mut app = opened();
    let _cmd = read(&mut app, fixture("in-progress"), 6);
    let running = app
        .transcript()
        .running()
        .map(TranscriptTurn::id)
        .expect("a running turn");
    type_text(&mut app, "second");
    assert_eq!(app.update(Msg::Submit(stamp(2))), sent(2, "second"));
    assert_eq!(app.draft(), "");
    let _cmd = answer(
        &mut app,
        Answer::MessageSent {
            message_id: message_id(2),
            result: Ok(SendMessageReply {
                turn_id: running,
                outcome: Outcome::Created,
            }),
        },
    );
    assert!(app.settled());
    let landed = format!(
        r#"{{"type":"data-envelope","data":{{"envelope":{{"user":"{}"}},"from":"user","text":"second"}}}}"#,
        message_id(2)
    );
    let _cmd = read(
        &mut app,
        vec![serde_json::from_str(&landed).expect("a chunk")],
        7,
    );
    assert_eq!(app.transcript().queued().len(), 1);
    assert!(screen(&mut app, 60, 12).contains("second  queued"));
    let carried = format!(
        r#"{{"type":"data-step-carries","id":"{running}","data":{{"envelopes":[{{"user":"{}"}}]}}}}"#,
        message_id(2)
    );
    let _cmd = read(
        &mut app,
        vec![serde_json::from_str(&carried).expect("a chunk")],
        8,
    );
    assert!(app.transcript().queued().is_empty(), "carried");
    let text = screen(&mut app, 60, 12);
    assert!(text.contains("› second") && !text.contains("queued"));
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
    let rejection = Rejection::MessageExistsWithDifferentText {
        message_id: message_id(3),
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
            .is_some_and(|notice| notice.contains("different text"))
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
    assert_eq!(app.offset(), Offset::at(6).expect("in range"));
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
            retry: Some(Millis::new(5)),
        },
    );
    let _cmd = stream(&mut app, StreamUpdate::Connected);
    assert!(!app.caught_up(), "reconnected, but nothing read yet");
    let from = app.offset();
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
    assert_eq!(app.offset(), Offset::BEGINNING);
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
            actor: session(),
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
    let other = turn_id(255);
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
            actor: session(),
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
            retry: Some(Millis::new(1)),
        },
    }));
    assert!(matches!(app.connection(), Connection::Broken(_)));
    assert_eq!(
        app.offset(),
        Offset::BEGINNING,
        "nothing past the broken read is acknowledged"
    );

    let cmd = app.update(Msg::Reconnect);
    assert_eq!(
        cmd,
        Cmd::Follow {
            follow: FollowId(3),
            actor: session(),
            from: Offset::BEGINNING,
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
            actor: session(),
            from: Offset::at(11).expect("in range"),
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
            retry: Some(Millis::new(500)),
        },
    );
    assert!(matches!(app.connection(), Connection::Retrying(reason) if reason.contains("500ms")));
}

/// ^r cuts a follower's backoff short, once: until the new follower
/// reports, more presses start nothing.
#[test]
fn ctrl_r_retries_at_once_while_retrying() {
    let mut app = opened();
    let _cmd = stream(
        &mut app,
        StreamUpdate::Disconnected {
            error: ClientError::ClosedEarly,
            retry: Some(Millis::new(500)),
        },
    );
    assert!(matches!(
        app.update(Msg::Reconnect),
        Cmd::Follow {
            follow: FollowId(2),
            ..
        }
    ));
    assert_eq!(app.connection(), &Connection::Connecting);
    assert_eq!(app.update(Msg::Reconnect), Cmd::None, "already connecting");
}

#[test]
fn the_draft_is_bounded() {
    let mut limits = Limits::DEFAULT;
    limits.wire.max_message_text_bytes = 4;
    let (mut app, _cmd) = App::init(Start::Open(session()), limits);
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
fn renders_a_new_actor_being_created() {
    let (mut app, _cmd) = App::init(Start::New(session()), Limits::DEFAULT);
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
        actor_id: session(),
        limit: 4,
    });
    assert_eq!(full.recovery(), Recovery::Retry);
    let actors = ClientError::Rejected(Rejection::TooManyActors { limit: 8 });
    assert_eq!(actors.recovery(), Recovery::Retry);
    let session_full = Rejection::ActorFull {
        actor_id: session(),
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

fn summary(byte: u8, name: &str, parent: Option<ActorId>, state: ActorState) -> ActorSummary {
    ActorSummary {
        actor_id: ActorId::from_parts(
            UnixMillis::new(1_760_000_000_000).expect("in range"),
            &[byte; 10],
        ),
        name: name.to_owned(),
        parent,
        depth: u32::from(parent.is_some()),
        state,
        approvals: Vec::new(),
        tasks: 0,
        unread: 0,
    }
}

/// The opened actor, a second root, and a child of the second: the tree
/// lists the child under its parent.
fn three_actors() -> Vec<ActorSummary> {
    let mut main = summary(1, "main", None, ActorState::Idle);
    main.actor_id = session();
    let other = summary(2, "other", None, ActorState::Running { turn_id: TurnId(3) });
    let mut child = summary(3, "helper", Some(other.actor_id), ActorState::Stopped);
    child.approvals = vec![CallId::parse("toolu_4").expect("a call id")];
    child.tasks = 2;
    vec![child, other, main]
}

/// The tick lists the actors, at most one listing at a time, and ticks on.
#[test]
fn the_tick_lists_the_actors_one_listing_at_a_time() {
    let mut app = opened();
    assert_eq!(
        app.update(Msg::Feedback(Feedback::Tick)),
        Cmd::Batch(vec![Cmd::None, Cmd::Tick(Limits::DEFAULT.refresh)]),
        "the listing from startup is still on its way"
    );
    let _cmd = answer(
        &mut app,
        Answer::Actors(Ok(ActorList {
            actors: three_actors(),
        })),
    );
    assert_eq!(app.actors().len(), 3);
    assert_eq!(
        app.update(Msg::Feedback(Feedback::Tick)),
        Cmd::Batch(vec![
            Cmd::Send(Command::ListActors),
            Cmd::Tick(Limits::DEFAULT.refresh)
        ])
    );
    let _cmd = answer(&mut app, Answer::Actors(Err(ClientError::ClosedEarly)));
    assert_eq!(app.actors().len(), 3, "a failed listing keeps the last");
    assert_eq!(app.notice(), None, "and says nothing");
}

/// Tab follows the next actor in tree order, from the start of its stream;
/// approvals and Esc then go to it.
#[test]
fn tab_follows_the_next_actor_in_the_tree() {
    let mut app = opened();
    assert_eq!(
        app.update(Msg::Switch(Direction::Next)),
        Cmd::None,
        "no other actor yet"
    );
    let _cmd = read(&mut app, fixture("simple"), 11);
    let _cmd = answer(
        &mut app,
        Answer::Actors(Ok(ActorList {
            actors: three_actors(),
        })),
    );
    let order: Vec<&str> = view::tree(app.actors())
        .into_iter()
        .map(|(_, actor)| actor.name.as_str())
        .collect();
    assert_eq!(
        order,
        ["other", "helper", "main"],
        "roots by id, children under parents"
    );
    assert!(matches!(
        key(&app, KeyCode::Tab, KeyModifiers::NONE),
        Some(Msg::Switch(Direction::Next))
    ));
    let other = three_actors()
        .get(1)
        .map(|actor| actor.actor_id)
        .expect("other");
    assert_eq!(
        app.update(Msg::Switch(Direction::Next)),
        Cmd::Follow {
            follow: FollowId(2),
            actor: other,
            from: Offset::BEGINNING,
        }
    );
    assert_eq!(app.actor(), other);
    assert!(app.transcript().turns().is_empty(), "its own stream");
    let _cmd = app.update(Msg::Switch(Direction::Previous));
    assert_eq!(app.actor(), session(), "and back");
}

#[test]
fn renders_the_actor_tree_and_running_tasks() {
    let mut app = opened();
    let chunks = fixture("steering");
    let started = chunks
        .iter()
        .position(|chunk| matches!(chunk, UiChunk::TaskStarted { .. }))
        .expect("a task starts");
    let _cmd = read(
        &mut app,
        chunks.get(..=started).expect("in range").to_vec(),
        1,
    );
    let _cmd = answer(
        &mut app,
        Answer::Actors(Ok(ActorList {
            actors: three_actors(),
        })),
    );
    insta::assert_snapshot!(screen(&mut app, 90, 16));
}

#[test]
fn ctrl_k_stops_the_actor_followed_only_once_confirmed() {
    let mut app = opened();
    let ask = key(&app, KeyCode::Char('k'), KeyModifiers::CONTROL).expect("a message");
    assert!(matches!(ask, Msg::AskStop(_)));
    assert_eq!(
        app.update(Msg::AskStop(stamp(4))),
        Cmd::None,
        "it asks first"
    );
    assert!(app.confirming_stop() && !app.settled());
    assert!(
        matches!(
            key(&app, KeyCode::Char('n'), KeyModifiers::NONE),
            Some(Msg::ConfirmStop(false))
        ),
        "the next key answers the question"
    );
    assert_eq!(app.update(Msg::ConfirmStop(false)), Cmd::None);
    assert!(app.settled(), "dropped");
    let _cmd = app.update(Msg::AskStop(stamp(5)));
    let stop = StopActor {
        stop_id: message_id(5),
    };
    assert_eq!(
        app.update(Msg::ConfirmStop(true)),
        Cmd::Send(Command::Stop {
            actor: session(),
            body: stop,
        })
    );
    assert!(!app.settled(), "on its way");
    let _cmd = answer(
        &mut app,
        Answer::Stopped {
            stop_id: stop.stop_id,
            result: Err(ClientError::Rejected(Rejection::ActorStopped {
                actor_id: session(),
            })),
        },
    );
    assert!(app.settled());
    assert!(
        app.notice()
            .is_some_and(|notice| notice.contains("stopped")),
        "{:?}",
        app.notice()
    );
}
