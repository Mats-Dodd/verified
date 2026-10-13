//! The transcript fold against transcripts the AI SDK itself computed.
//!
//! Each fixture in `tests/fixtures/sessions` is a session stream
//! (`<name>.jsonl`, one chunk per line) and the turns `readUIMessageStream`
//! from `ai@7.0.127` makes of it (`<name>.turns.json`). The first six are q's
//! fixtures, copied unchanged but for `steps`, which lost its reasoning part
//! when the wire vocabulary was cut to what the daemon emits; `tools` covers
//! tool parts and approvals.
//! Regenerate the goldens with `tests/fixtures/golden.ts`.

use proptest::prelude::*;
use serde_json::Value;
use verified_wire::WireLimits;
use verified_wire::chunk::UiChunk;
use verified_wire::sse::SseDecoder;
use verified_wire::stream::{StreamEvent, decode_chunks};
use verified_wire::transcript::{FoldError, ToolState, Transcript};

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/sessions/", $name, ".jsonl")),
            include_str!(concat!("fixtures/sessions/", $name, ".turns.json")),
        )
    };
}

const FIXTURES: [(&str, &str, &str); 7] = [
    fixture!("cancelled"),
    fixture!("conversation"),
    fixture!("failed"),
    fixture!("in-progress"),
    fixture!("simple"),
    fixture!("steps"),
    fixture!("tools"),
];

fn chunks(stream: &str) -> Vec<UiChunk> {
    stream
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("a chunk"))
        .collect()
}

fn fold(chunks: &[UiChunk]) -> Transcript {
    let mut transcript = Transcript::new(&WireLimits::DEFAULT);
    for chunk in chunks {
        transcript.apply(chunk).expect("the chunk folds");
    }
    transcript
}

fn fixture(name: &str) -> Vec<UiChunk> {
    let (_, stream, _) = FIXTURES
        .iter()
        .find(|(fixture, _, _)| *fixture == name)
        .expect("a fixture");
    chunks(stream)
}

/// Every fixture stream folds to the transcript the AI SDK makes of it.
#[test]
fn matches_the_ai_sdk_on_every_golden_fixture() {
    for (name, stream, golden) in FIXTURES {
        let transcript = fold(&chunks(stream));
        let expected: Value = serde_json::from_str(golden).expect("a golden transcript");
        let actual = serde_json::to_value(transcript.turns()).expect("turns serialize");
        assert_eq!(actual, expected, "{name}");
    }
}

/// Decoding then encoding a chunk loses nothing the protocol carries.
#[test]
fn every_fixture_chunk_round_trips() {
    for (name, stream, _) in FIXTURES {
        for line in stream.lines().filter(|line| !line.is_empty()) {
            let chunk: UiChunk = serde_json::from_str(line).expect("a chunk");
            let original: Value = serde_json::from_str(line).expect("JSON");
            let encoded = serde_json::to_value(&chunk).expect("chunks serialize");
            assert_eq!(encoded, original, "{name}: {line}");
        }
    }
}

#[test]
fn the_last_turn_runs_until_it_ends() {
    let transcript = fold(&fixture("in-progress"));
    let running = transcript.running().expect("a running turn");
    assert_eq!(running.model(), Some("claude-haiku-4-5"));
    assert_eq!(
        fold(&fixture("simple"))
            .running()
            .map(verified_wire::transcript::TranscriptTurn::id),
        None
    );

    let tools = fold(&fixture("tools"));
    let waiting = tools.running().expect("a turn awaiting approval");
    let reply = waiting.reply().expect("a reply");
    assert!(
        reply.parts.iter().any(|part| matches!(
            part,
            verified_wire::transcript::Part::Tool(tool) if tool.state == ToolState::ApprovalRequested
        )),
        "the bash call awaits approval"
    );
    assert_eq!(
        tools
            .turns()
            .first()
            .and_then(verified_wire::transcript::TranscriptTurn::message_id)
            .map(|id| id.to_string()),
        Some("01a10400-0000-7000-8000-0000000000a1".to_owned())
    );
}

fn decode(line: &str) -> UiChunk {
    serde_json::from_str(line).expect("a chunk")
}

/// A chunk that does not fit is an error and changes nothing.
#[test]
fn rejects_chunks_that_do_not_fit_without_changing_anything() {
    let mut transcript = Transcript::new(&WireLimits::DEFAULT);
    assert_eq!(
        transcript.apply(&UiChunk::StartStep),
        Err(FoldError::BeforeFirstTurn {
            chunk: "start-step"
        })
    );
    let mut transcript = fold(&fixture("in-progress"));
    let before = serde_json::to_value(transcript.turns()).expect("turns serialize");
    for line in [
        r#"{"type":"text-delta","id":"t9","delta":"x"}"#,
        r#"{"type":"text-end","id":"t9"}"#,
        r#"{"type":"tool-input-delta","toolCallId":"toolu_1","inputTextDelta":"{"}"#,
        r#"{"type":"tool-output-available","toolCallId":"toolu_1","output":null}"#,
        r#"{"type":"tool-approval-response","approvalId":"a","approved":true}"#,
        r#"{"type":"data-turn-end","id":"01a10400-0000-7000-8000-000000000009","data":{"reason":"failed"}}"#,
    ] {
        assert!(
            transcript.apply(&decode(line)).is_err(),
            "{line} should not fold"
        );
        assert_eq!(
            serde_json::to_value(transcript.turns()).expect("turns serialize"),
            before,
            "{line} changed the transcript"
        );
    }
}

#[test]
fn bounds_turns_parts_part_text_and_errors() {
    let tight = WireLimits {
        max_turns: 1,
        max_parts_per_reply: 2,
        max_part_bytes: 4,
        max_errors_per_turn: 1,
        ..WireLimits::DEFAULT
    };
    let mut transcript = Transcript::new(&tight);
    let user = r#"{"type":"data-user-message","id":"01a10400-0000-7000-8000-000000000001","data":{"text":"a"}}"#;
    transcript
        .apply(&decode(user))
        .expect("the first turn fits");
    assert_eq!(
        transcript.apply(&decode(user)),
        Err(FoldError::TooManyTurns { limit: 1 })
    );
    transcript.apply(&UiChunk::StartStep).expect("one part");
    transcript
        .apply(&decode(r#"{"type":"text-start","id":"t0"}"#))
        .expect("two parts");
    assert_eq!(
        transcript.apply(&UiChunk::StartStep),
        Err(FoldError::TooManyParts { limit: 2 })
    );
    transcript
        .apply(&decode(r#"{"type":"text-delta","id":"t0","delta":"abcd"}"#))
        .expect("four bytes fit");
    assert_eq!(
        transcript.apply(&decode(r#"{"type":"text-delta","id":"t0","delta":"e"}"#)),
        Err(FoldError::PartTooLarge { limit: 4 })
    );
    let error = decode(r#"{"type":"error","errorText":"x"}"#);
    transcript.apply(&error).expect("one error fits");
    assert_eq!(
        transcript.apply(&error),
        Err(FoldError::TooManyErrors { limit: 1 })
    );
}

/// The fixture streams, sent as Durable Streams SSE `data` events grouped at
/// `groups` and delivered in pieces cut at `cuts`.
fn over_the_wire(chunks: &[UiChunk], groups: &[usize], cuts: &[usize]) -> Vec<UiChunk> {
    let mut wire = String::new();
    let mut rest = chunks;
    let mut sizes = groups.iter().cycle();
    while !rest.is_empty() {
        let size = sizes.next().copied().unwrap_or(1).clamp(1, rest.len());
        let (group, tail) = rest.split_at(size);
        StreamEvent::Data(group.to_vec())
            .to_sse()
            .encode_into(&mut wire);
        rest = tail;
    }
    let bytes = wire.as_bytes();
    let mut ends: Vec<usize> = cuts
        .iter()
        .map(|cut| cut.checked_rem(bytes.len().saturating_add(1)).unwrap_or(0))
        .collect();
    ends.sort_unstable();
    ends.push(bytes.len());
    let mut decoder = SseDecoder::new(&WireLimits::DEFAULT);
    let mut received = Vec::new();
    let mut start = 0;
    for end in ends {
        for event in decoder.feed(bytes.get(start..end).unwrap_or_default()) {
            let event = event.expect("a well-formed stream");
            match StreamEvent::from_sse(&event).expect("a stream event") {
                StreamEvent::Data(group) => received.extend(group),
                StreamEvent::Control(_) => {}
            }
        }
        start = end;
    }
    received
}

proptest! {
    /// However the daemon groups chunks into events and however the network
    /// splits the bytes, the TUI folds the same transcript.
    #[test]
    fn the_wire_never_changes_the_transcript(
        fixture_index in 0..FIXTURES.len(),
        groups in prop::collection::vec(1usize..6, 1..6),
        cuts in prop::collection::vec(any::<usize>(), 0..12),
    ) {
        let (_, stream, _) = FIXTURES.get(fixture_index).copied().expect("in range");
        let sent = chunks(stream);
        let received = over_the_wire(&sent, &groups, &cuts);
        prop_assert_eq!(&received, &sent);
        prop_assert_eq!(
            serde_json::to_value(fold(&received).turns()).expect("turns serialize"),
            serde_json::to_value(fold(&sent).turns()).expect("turns serialize")
        );
    }

    /// A data event's chunk array is the same chunks.
    #[test]
    fn chunk_arrays_round_trip(fixture_index in 0..FIXTURES.len()) {
        let (_, stream, _) = FIXTURES.get(fixture_index).copied().expect("in range");
        let sent = chunks(stream);
        let body = verified_wire::stream::encode_chunks(&sent);
        prop_assert_eq!(decode_chunks(&body).ok(), Some(sent));
    }
}
