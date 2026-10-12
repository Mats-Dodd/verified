//! In-process tests of the shell: the real core, shell, projection, history,
//! tools and log framing, with the scripted model, an in-memory environment
//! and an in-memory disk. No sockets and no filesystem, so they run the same
//! with and without the `sim` feature.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::id::{CallId, IdEntropy, MessageId, SessionId, TurnId, UnixMillis};
use verified_domain::{Cause, Exit, Limits, LimitsConfig, Millis, Record, TurnOutcome};
use verified_server::daemon::{self, DaemonError, Registry};
use verified_server::env::{
    BashGrant, DirEntry, EditGrant, EntryKind, Env, EnvError, ExecOptions, ExitStatus, Stopped,
    Written,
};
use verified_server::limits::ServerLimits;
use verified_server::log::{MemDisk, MemLogStore};
use verified_server::model::fake::FakeModel;
use verified_server::payload::{Parsed, ProjectPath};
use verified_server::projection::Projector;
use verified_server::seam::{Clock, OsEntropy, SystemClock, World};
use verified_server::shell::{CommandError, Seams, SessionHandle, ShellConfig};
use verified_wire::WireLimits;
use verified_wire::chunk::{TurnEndReason, UiChunk};
use verified_wire::command::{
    Approve, Cancel, MessageText, Outcome, Rejection, SendMessage, Verdict,
};
use verified_wire::stream::{Offset, ReadFrom};
use verified_wire::transcript::Transcript;

/// `work`, or an error once `limit` passes: a test's own bound on how long
/// it waits for the code under test, never a seam of that code.
#[expect(
    clippy::disallowed_methods,
    reason = "a test's own deadline on waiting, not a timeout inside the code under test"
)]
async fn within<F: Future>(
    limit: Duration,
    work: F,
) -> Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(limit, work).await
}

/// Files and a shell in memory. `block` commands wait for their cancel.
#[derive(Debug, Default)]
struct MemEnv {
    #[expect(
        clippy::disallowed_types,
        reason = "test env: tool tasks write, the test reads after a turn ends; dropped with the test"
    )]
    files: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
    #[expect(
        clippy::disallowed_types,
        reason = "test env: exec appends, the test reads after a turn ends; dropped with the test"
    )]
    commands: std::sync::Mutex<Vec<(String, String)>>,
    #[expect(
        clippy::disallowed_types,
        reason = "test env: edit tasks append, the test reads after a turn ends; dropped with the test"
    )]
    writes: std::sync::Mutex<Vec<String>>,
    /// Writes never return: a crash between dispatch and result.
    stall_writes: bool,
}

impl MemEnv {
    fn with_file(path: &str, text: &str) -> Arc<Self> {
        Arc::new(Self::holding(path, text, false))
    }

    /// An env with one file, whose writes stall if `stall`.
    fn holding(path: &str, text: &str, stall: bool) -> Self {
        let env = Self {
            stall_writes: stall,
            ..Self::default()
        };
        env.files
            .lock()
            .expect("unpoisoned")
            .insert(path.to_owned(), text.as_bytes().to_vec());
        env
    }

    fn file(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .expect("unpoisoned")
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    fn commands(&self) -> Vec<(String, String)> {
        self.commands.lock().expect("unpoisoned").clone()
    }

    /// The paths of the writes that landed, in order.
    fn writes(&self) -> Vec<String> {
        self.writes.lock().expect("unpoisoned").clone()
    }
}

#[async_trait]
impl Env for MemEnv {
    async fn read(&self, path: &ProjectPath, _limit: u64) -> Result<Vec<u8>, EnvError> {
        self.files
            .lock()
            .expect("unpoisoned")
            .get(path.as_str())
            .cloned()
            .ok_or_else(|| EnvError::NotFound {
                path: path.as_str().to_owned(),
            })
    }

    async fn list(
        &self,
        _path: Option<&ProjectPath>,
        _limit: usize,
    ) -> Result<Vec<DirEntry>, EnvError> {
        Ok(self
            .files
            .lock()
            .expect("unpoisoned")
            .keys()
            .map(|name| DirEntry {
                name: name.clone(),
                kind: EntryKind::File,
            })
            .collect())
    }

    async fn write(&self, edit: &EditGrant<'_>, contents: &[u8]) -> Result<Written, EnvError> {
        if self.stall_writes {
            core::future::pending::<()>().await;
        }
        let path = edit.path().as_str().to_owned();
        self.files
            .lock()
            .expect("unpoisoned")
            .insert(path.clone(), contents.to_vec());
        self.writes.lock().expect("unpoisoned").push(path);
        Ok(Written::Synced)
    }

    async fn exec(
        &self,
        bash: &BashGrant<'_>,
        options: &ExecOptions,
        output: mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<ExitStatus, EnvError> {
        let command = bash.command();
        let call = options
            .env
            .iter()
            .find(|(key, _)| key == "VERIFIED_CALL_ID")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(
            call,
            bash.id().as_str(),
            "the command sees the id of the call that was approved"
        );
        self.commands
            .lock()
            .expect("unpoisoned")
            .push((command.as_str().to_owned(), call));
        if command.as_str() == "block" {
            cancel.cancelled().await;
            return Ok(ExitStatus::Stopped(Stopped::Cancelled));
        }
        output
            .send(format!("ran {}\n", command.as_str()).into_bytes())
            .await
            .expect("the collector is listening");
        Ok(ExitStatus::Code(0))
    }
}

struct Daemon {
    registry: Registry,
    world: World,
    _fatal: mpsc::Receiver<DaemonError>,
}

fn limits() -> ServerLimits {
    let mut limits = ServerLimits::v0();
    limits.core = Limits::parse(LimitsConfig {
        retry_schedule: vec![Millis::new(1), Millis::new(1)],
        ..LimitsConfig::v0()
    })
    .expect("consistent limits");
    limits
}

async fn boot(disk: &MemDisk, env: Arc<dyn Env>) -> Daemon {
    boot_with(disk, env, limits()).await
}

async fn boot_with(disk: &MemDisk, env: Arc<dyn Env>, limits: ServerLimits) -> Daemon {
    let world = World::new();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let seams = Seams {
        model: Arc::new(FakeModel::new(
            Arc::clone(&clock),
            limits.model_parts_per_call,
        )),
        env,
        clock,
        entropy: Arc::new(OsEntropy),
        world: world.clone(),
    };
    let config = Arc::new(ShellConfig {
        limits: limits.clone(),
        model: "fake".to_owned(),
        system: "You are a test.".to_owned(),
        planted: verified_server::shell::Planted::default(),
    });
    let (fatal, failed) = mpsc::channel(1);
    let store = Arc::new(MemLogStore::new(
        disk.clone(),
        limits.log_line_bytes,
        limits.log_file_bytes,
    ));
    let registry = daemon::boot(store, seams, config, fatal)
        .await
        .expect("the daemon boots");
    Daemon {
        registry,
        world,
        _fatal: failed,
    }
}

fn session_id() -> SessionId {
    SessionId::from_parts(
        UnixMillis::new(1_700_000_000_000).expect("a time"),
        &[7; 10],
    )
}

fn message_id(seed: u8) -> MessageId {
    let entropy: IdEntropy = [seed; 10];
    MessageId::from_parts(
        UnixMillis::new(1_700_000_000_001).expect("a time"),
        &entropy,
    )
}

fn send(seed: u8, text: &str) -> SendMessage {
    SendMessage {
        message_id: message_id(seed),
        text: MessageText::parse(text.to_owned(), &WireLimits::DEFAULT).expect("text"),
    }
}

async fn all_chunks(handle: &SessionHandle) -> Vec<UiChunk> {
    let mut chunks = Vec::new();
    let mut from = Offset::BEGINNING;
    loop {
        let read = handle
            .read(ReadFrom::Offset(from))
            .await
            .expect("the session runs");
        let done = read.next == from;
        chunks.extend(read.chunks);
        if done {
            return chunks;
        }
        from = read.next;
    }
}

/// Waits until `ready` holds for the stream.
async fn until<F>(handle: &SessionHandle, ready: F) -> Vec<UiChunk>
where
    F: Fn(&[UiChunk]) -> bool,
{
    let tail = handle.tail();
    loop {
        let seen = tail.get();
        let chunks = all_chunks(handle).await;
        if ready(&chunks) {
            return chunks;
        }
        within(Duration::from_secs(20), tail.past(seen))
            .await
            .expect("the stream moves within 20s")
            .expect("the session runs");
    }
}

fn ended(turn: TurnId) -> impl Fn(&[UiChunk]) -> bool {
    move |chunks| {
        chunks
            .iter()
            .any(|chunk| matches!(chunk, UiChunk::TurnEnded { id, .. } if *id == turn))
    }
}

fn approval_requested(chunks: &[UiChunk]) -> Option<CallId> {
    chunks.iter().find_map(|chunk| {
        if let UiChunk::ToolApprovalRequest {
            tool_call_id,
            is_automatic: None,
            ..
        } = chunk
        {
            Some(tool_call_id.clone())
        } else {
            None
        }
    })
}

/// Folds the stream like the TUI and checks the projection is a pure
/// function of the log.
async fn fold(handle: &SessionHandle) -> Transcript {
    let chunks = all_chunks(handle).await;
    let mut transcript = Transcript::new(&WireLimits::DEFAULT);
    for chunk in &chunks {
        transcript
            .apply(chunk)
            .map_err(|error| format!("{} does not fold: {error}", chunk.kind()))
            .expect("every projected chunk folds");
    }
    let records = handle.records().await.expect("the session runs");
    let mut projector = Projector::new("fake".to_owned());
    let replayed: Vec<UiChunk> = records
        .iter()
        .flat_map(|record| projector.project(record))
        .collect();
    assert_eq!(replayed, chunks, "the stream is the projection of the log");
    transcript
}

fn reply_text(transcript: &Transcript, turn: usize) -> String {
    let turn = transcript.turns().get(turn).expect("the turn");
    turn.reply()
        .map(|reply| {
            reply
                .parts
                .iter()
                .filter_map(|part| {
                    if let verified_wire::transcript::Part::Text { text, .. } = part {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .unwrap_or_default()
}

async fn records(handle: &SessionHandle) -> Vec<Record<Parsed>> {
    handle.records().await.expect("the session runs")
}

#[tokio::test]
async fn text_turn_streams_and_repeats_are_exists() {
    let disk = MemDisk::new();
    let daemon = boot(&disk, Arc::new(MemEnv::default())).await;
    let (outcome, handle) = daemon.registry.create(session_id()).await.expect("created");
    assert_eq!(outcome, Outcome::Created, "a new session");
    let (again, _) = daemon.registry.create(session_id()).await.expect("exists");
    assert_eq!(again, Outcome::Exists, "a repeat create is exists");

    let reply = handle
        .send_message(send(1, "hello there"))
        .await
        .expect("accepted");
    assert_eq!(reply.outcome, Outcome::Created, "a new message");
    until(&handle, ended(reply.turn_id)).await;

    let repeat = handle
        .send_message(send(1, "hello there"))
        .await
        .expect("a repeat");
    assert_eq!(repeat.outcome, Outcome::Exists, "same id, same text");
    assert_eq!(repeat.turn_id, reply.turn_id, "the original turn");
    let conflict = handle.send_message(send(1, "something else")).await;
    assert!(
        matches!(
            conflict,
            Err(CommandError::Rejected(
                Rejection::MessageExistsWithDifferentText { .. }
            ))
        ),
        "same id, different text: {conflict:?}"
    );

    let transcript = fold(&handle).await;
    assert_eq!(transcript.turns().len(), 1, "one turn");
    assert_eq!(
        reply_text(&transcript, 0),
        "Echo: hello there",
        "the scripted echo"
    );
    let second = handle
        .send_message(send(2, "again"))
        .await
        .expect("accepted");
    until(&handle, ended(second.turn_id)).await;
    let transcript = fold(&handle).await;
    assert_eq!(reply_text(&transcript, 1), "Echo: again", "second turn");
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn safe_read_runs_without_asking() {
    let env = MemEnv::with_file("notes.txt", "the answer is 42");
    let daemon = boot(&MemDisk::new(), env).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "read notes.txt"))
        .await
        .expect("accepted");
    let chunks = until(&handle, ended(reply.turn_id)).await;
    assert!(
        chunks.iter().any(|chunk| matches!(
            chunk,
            UiChunk::ToolApprovalRequest {
                is_automatic: Some(true),
                ..
            }
        )),
        "policy approval is projected as automatic"
    );
    let transcript = fold(&handle).await;
    assert!(
        reply_text(&transcript, 0).contains("Result: the answer is 42"),
        "the model saw the file: {}",
        reply_text(&transcript, 0)
    );
    let log = records(&handle).await;
    let order: Vec<&str> = log
        .iter()
        .filter_map(|record| {
            if matches!(record, Record::Approved { .. }) {
                Some("approved")
            } else if matches!(record, Record::Dispatched { .. }) {
                Some("dispatched")
            } else if matches!(record, Record::ToolResult { .. }) {
                Some("result")
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        order,
        vec!["approved", "dispatched", "result"],
        "approve before dispatch before result"
    );
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn edit_waits_for_approval_then_applies_once() {
    let env = MemEnv::with_file("lib.rs", "fn mian() {}");
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "edit lib.rs mian main"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    assert_eq!(
        env.file("lib.rs").as_deref(),
        Some("fn mian() {}"),
        "nothing changes before approval"
    );
    let grant = Approve {
        call_id: call_id.clone(),
        verdict: Verdict::Grant,
    };
    let granted = handle.approve(grant.clone()).await.expect("granted");
    assert_eq!(granted.outcome, Outcome::Created, "a new verdict");
    until(&handle, ended(reply.turn_id)).await;
    assert_eq!(
        env.file("lib.rs").as_deref(),
        Some("fn main() {}"),
        "edited"
    );
    let repeat = handle.approve(grant).await.expect("a repeat");
    assert_eq!(repeat.outcome, Outcome::Exists, "same verdict again");
    let flip = handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Deny,
        })
        .await;
    assert!(
        matches!(
            flip,
            Err(CommandError::Rejected(
                Rejection::ApprovalExistsWithDifferentVerdict {
                    verdict: Verdict::Grant,
                    ..
                }
            ))
        ),
        "the other verdict conflicts: {flip:?}"
    );
    let transcript = fold(&handle).await;
    assert!(
        reply_text(&transcript, 0).contains("Result: Edited lib.rs."),
        "{}",
        reply_text(&transcript, 0)
    );
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn denied_bash_never_runs() {
    let env = Arc::new(MemEnv::default());
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "bash rm -rf target"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Deny,
        })
        .await
        .expect("denied");
    let chunks = until(&handle, ended(reply.turn_id)).await;
    assert!(env.commands().is_empty(), "a denied command never runs");
    assert!(
        chunks
            .iter()
            .any(|chunk| matches!(chunk, UiChunk::ToolOutputDenied { .. })),
        "projected as denied"
    );
    let transcript = fold(&handle).await;
    assert!(
        reply_text(&transcript, 0).contains("Error: The user denied this tool call."),
        "the model heard the refusal: {}",
        reply_text(&transcript, 0)
    );
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn approved_bash_sees_its_call_id() {
    let env = Arc::new(MemEnv::default());
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "bash cargo test"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    handle
        .approve(Approve {
            call_id: call_id.clone(),
            verdict: Verdict::Grant,
        })
        .await
        .expect("granted");
    until(&handle, ended(reply.turn_id)).await;
    assert_eq!(
        env.commands(),
        vec![("cargo test".to_owned(), call_id.as_str().to_owned())],
        "ran once, with VERIFIED_CALL_ID"
    );
    fold(&handle).await;
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn cancel_stops_a_streaming_turn() {
    let daemon = boot(&MemDisk::new(), Arc::new(MemEnv::default())).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "slow please"))
        .await
        .expect("accepted");
    until(&handle, |chunks| {
        chunks
            .iter()
            .any(|chunk| matches!(chunk, UiChunk::TextDelta { .. }))
    })
    .await;
    let cancelled = handle
        .cancel(Cancel {
            turn_id: reply.turn_id,
        })
        .await
        .expect("cancelling");
    assert_eq!(cancelled.turn_id, reply.turn_id, "the running turn");
    assert_eq!(cancelled.outcome, Outcome::Created, "a new cancel");
    until(&handle, ended(reply.turn_id)).await;
    let again = handle
        .cancel(Cancel {
            turn_id: reply.turn_id,
        })
        .await
        .expect("a repeat");
    assert_eq!(again.outcome, Outcome::Exists, "cancelling twice is exists");
    let transcript = fold(&handle).await;
    assert_eq!(
        transcript
            .turns()
            .first()
            .and_then(verified_wire::transcript::TranscriptTurn::end),
        Some(TurnEndReason::Cancelled),
        "ended cancelled"
    );
    let other = TurnId::from_parts(
        UnixMillis::new(1_700_000_000_000).expect("a time"),
        &[9; 10],
    );
    let nothing = handle.cancel(Cancel { turn_id: other }).await;
    assert!(
        matches!(
            nothing,
            Err(CommandError::Rejected(Rejection::TurnNotRunning { turn_id })) if turn_id == other
        ),
        "a cancel names its turn; another turn is not running: {nothing:?}"
    );
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn overloaded_retries_then_fails() {
    let daemon = boot(&MemDisk::new(), Arc::new(MemEnv::default())).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "fail always"))
        .await
        .expect("accepted");
    let chunks = until(&handle, ended(reply.turn_id)).await;
    let resets = chunks
        .iter()
        .filter(|chunk| matches!(chunk, UiChunk::ResetStep))
        .count();
    assert_eq!(resets, 2, "one reset-step per retry in the schedule");
    let log = records(&handle).await;
    assert!(
        log.iter().any(|record| matches!(
            record,
            Record::TurnEnded {
                outcome: TurnOutcome::Failed(verified_domain::Failure::Model(Cause::Failed(
                    verified_domain::Fault::Overloaded
                ))),
                ..
            }
        )),
        "failed once the schedule ran out"
    );
    let transcript = fold(&handle).await;
    assert_eq!(
        transcript
            .turns()
            .first()
            .and_then(verified_wire::transcript::TranscriptTurn::end),
        Some(TurnEndReason::Failed),
        "ended failed"
    );
    // The next turn's history drops the failed attempts' partial text.
    let next = handle
        .send_message(send(2, "after"))
        .await
        .expect("accepted");
    until(&handle, ended(next.turn_id)).await;
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn parallel_reads_each_get_one_result() {
    let env = Arc::new(MemEnv::default());
    for name in ["a.txt", "b.txt"] {
        env.files
            .lock()
            .expect("unpoisoned")
            .insert(name.to_owned(), name.as_bytes().to_vec());
    }
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "read a.txt ; read b.txt"))
        .await
        .expect("accepted");
    until(&handle, ended(reply.turn_id)).await;
    let log = records(&handle).await;
    let results = log
        .iter()
        .filter(|record| matches!(record, Record::ToolResult { exit: Exit::Ok, .. }))
        .count();
    assert_eq!(results, 2, "both reads ran and resolved");
    fold(&handle).await;
    daemon.world.shutdown().cancel();
}

#[tokio::test]
async fn restart_mid_bash_resolves_unknown_and_never_reruns() {
    let disk = MemDisk::new();
    let env = Arc::new(MemEnv::default());
    let first = boot(&disk, Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = first.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "bash block"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Grant,
        })
        .await
        .expect("granted");
    // Wait until the command is running, then "crash" the daemon.
    for _ in 0..200u32 {
        if !env.commands().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(env.commands().len(), 1, "bash started");
    first.world.shutdown().cancel();
    drop(handle);

    let second = boot(&disk, Arc::<MemEnv>::clone(&env)).await;
    let handle = second
        .registry
        .lookup(session_id())
        .await
        .expect("recovered");
    until(&handle, ended(reply.turn_id)).await;
    let log = records(&handle).await;
    assert!(
        log.iter().any(|record| matches!(
            record,
            Record::ToolResult {
                exit: Exit::Err(Cause::Unknown),
                output: None,
                ..
            }
        )),
        "a dispatched bash without a result is Unknown after a restart"
    );
    assert_eq!(env.commands().len(), 1, "bash never runs twice");
    assert_eq!(
        log.iter()
            .filter(|record| matches!(record, Record::EpochBumped { .. }))
            .count(),
        2,
        "one epoch per start"
    );
    let transcript = fold(&handle).await;
    assert!(
        reply_text(&transcript, 0).contains("Unknown outcome"),
        "the model heard: {}",
        reply_text(&transcript, 0)
    );
    second.world.shutdown().cancel();
}

#[tokio::test]
async fn torn_tail_is_cut_and_corruption_refused() {
    let disk = MemDisk::new();
    let first = boot(&disk, Arc::new(MemEnv::default())).await;
    let (_, handle) = first.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "hello"))
        .await
        .expect("accepted");
    until(&handle, ended(reply.turn_id)).await;
    let committed = records(&handle).await;
    first.world.shutdown().cancel();
    drop(handle);

    // A torn group commit: half a line after the last whole record.
    let mut bytes = disk.bytes(session_id()).expect("the log");
    let whole = bytes.len();
    bytes.extend_from_slice(b"0badc0de 00000000000000");
    disk.set_bytes(session_id(), bytes.clone());
    let second = boot(&disk, Arc::new(MemEnv::default())).await;
    let handle = second
        .registry
        .lookup(session_id())
        .await
        .expect("recovered");
    let recovered = records(&handle).await;
    assert_eq!(
        recovered.get(..committed.len()),
        Some(committed.as_slice()),
        "every acknowledged record survives"
    );
    second.world.shutdown().cancel();
    drop(handle);

    // Damage inside an acknowledged group, followed by later groups.
    let mut damaged = disk.bytes(session_id()).expect("the log");
    assert!(damaged.len() > whole, "the restart appended an epoch");
    if let Some(byte) = damaged.get_mut(30) {
        *byte ^= 0x20;
    }
    disk.set_bytes(session_id(), damaged);
    let (fatal, _failed) = mpsc::channel(1);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let seams = Seams {
        model: Arc::new(FakeModel::new(Arc::clone(&clock), 100)),
        env: Arc::new(MemEnv::default()),
        clock,
        entropy: Arc::new(OsEntropy),
        world: World::new(),
    };
    let config = Arc::new(ShellConfig {
        limits: limits(),
        model: "fake".to_owned(),
        system: String::new(),
        planted: verified_server::shell::Planted::default(),
    });
    let refused = daemon::boot(
        Arc::new(MemLogStore::new(disk.clone(), 1 << 20, 1 << 30)),
        seams,
        config,
        fatal,
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(DaemonError::Log(
                verified_server::log::LogError::Corrupt { .. }
            ))
        ),
        "mid-log damage stops recovery: {:?}",
        refused.err()
    );
}

/// Polls `ready` for up to two seconds.
async fn eventually<F: Fn() -> bool>(ready: F) -> bool {
    for _ in 0..200u32 {
        if ready() {
            return true;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    ready()
}

/// A crash while an approved edit runs (dispatched, its write not yet
/// landed) resolves it `Unknown` after the restart, and it is never rerun:
/// an edit is `Unsafe`.
#[tokio::test]
async fn a_crash_during_an_edit_leaves_unknown_and_never_reruns() {
    let disk = MemDisk::new();
    let stalled = Arc::new(MemEnv::holding("lib.rs", "fn mian() {}", true));
    let first = boot(&disk, Arc::<MemEnv>::clone(&stalled)).await;
    let (_, handle) = first.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "edit lib.rs mian main"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Grant,
        })
        .await
        .expect("granted");
    // The edit is dispatched and stuck writing: "crash" the daemon there.
    let dispatched = eventually(|| {
        disk.bytes(session_id())
            .is_some_and(|bytes| String::from_utf8_lossy(&bytes).contains("\"Dispatched\""))
    })
    .await;
    assert!(dispatched, "the edit was dispatched");
    first.world.shutdown().cancel();
    drop(handle);

    let env = MemEnv::with_file("lib.rs", "fn mian() {}");
    let second = boot(&disk, Arc::<MemEnv>::clone(&env)).await;
    let handle = second
        .registry
        .lookup(session_id())
        .await
        .expect("recovered");
    until(&handle, ended(reply.turn_id)).await;
    assert!(env.writes().is_empty(), "the edit is not rerun");
    assert_eq!(env.file("lib.rs").as_deref(), Some("fn mian() {}"));
    let log = records(&handle).await;
    assert!(
        log.iter().any(|record| matches!(
            record,
            Record::ToolResult {
                exit: Exit::Err(Cause::Unknown),
                output: None,
                ..
            }
        )),
        "its outcome is unknown"
    );
    let transcript = fold(&handle).await;
    assert!(
        reply_text(&transcript, 0).contains("Unknown outcome"),
        "the model heard: {}",
        reply_text(&transcript, 0)
    );
    second.world.shutdown().cancel();
}

/// An approved edit of the longest path the tools accept runs without
/// crashing the daemon (a path whose staged sibling could not be named once
/// panicked inside the tool task); one byte longer and the path is refused
/// at the model boundary.
#[tokio::test]
async fn the_longest_path_edits_without_a_crash() {
    let path = format!(
        "{}/{}",
        "d".repeat(64),
        "n".repeat(ProjectPath::MAX_BYTES - 65)
    );
    let env = MemEnv::with_file(&path, "alpha");
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, &format!("edit {path} alpha beta")))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Grant,
        })
        .await
        .expect("granted");
    until(&handle, ended(reply.turn_id)).await;
    assert_eq!(env.file(&path).as_deref(), Some("beta"), "edited");

    let too_long = "n".repeat(ProjectPath::MAX_BYTES + 1);
    let invalid = handle
        .send_message(send(2, &format!("edit {too_long} alpha beta")))
        .await
        .expect("accepted");
    until(&handle, ended(invalid.turn_id)).await;
    let transcript = fold(&handle).await;
    assert!(
        reply_text(&transcript, 1).contains("Invalid tool call"),
        "{}",
        reply_text(&transcript, 1)
    );
    daemon.world.shutdown().cancel();
}

/// Cancelling a turn while its approved `bash` runs stops the command, and
/// the model hears one honest message: stopped, and what it started may
/// still be running.
#[tokio::test]
async fn cancelling_bash_reports_the_honest_message() {
    let env = Arc::new(MemEnv::default());
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "bash block"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Grant,
        })
        .await
        .expect("granted");
    assert!(
        eventually(|| !env.commands().is_empty()).await,
        "bash started"
    );
    handle
        .cancel(Cancel {
            turn_id: reply.turn_id,
        })
        .await
        .expect("cancelling");
    until(&handle, ended(reply.turn_id)).await;
    let log = records(&handle).await;
    let output = log.iter().find_map(|record| {
        if let Record::ToolResult {
            exit: Exit::Err(Cause::Interrupted),
            output: Some(output),
            ..
        } = record
        {
            Some(output.body.text.clone())
        } else {
            None
        }
    });
    assert_eq!(
        output.as_deref(),
        Some("[stopped (cancelled); processes it started may still be running]"),
        "{log:?}"
    );
    assert_eq!(env.commands().len(), 1, "ran once");
    fold(&handle).await;
    daemon.world.shutdown().cancel();
}

/// A log that reaches its file bound refuses the append that would pass it:
/// nothing of it is written, the session refuses commands from then on,
/// and the log still replays to exactly what was acknowledged, including
/// after a restart, which serves it read-only instead of crashing.
#[tokio::test]
async fn a_full_log_stops_taking_records_and_loses_nothing() {
    let mut small = limits();
    small.wire.max_message_text_bytes = 1_000;
    small.tool_output_bytes = 1_000;
    small.log_line_bytes = 16usize << 10u32;
    small.log_file_bytes = 16u64 << 10u32;
    let disk = MemDisk::new();
    let first = boot_with(&disk, Arc::new(MemEnv::default()), small.clone()).await;
    let (_, handle) = first.registry.create(session_id()).await.expect("created");
    let mut full = None;
    for seed in 1..=60u8 {
        match handle
            .send_message(send(seed, &format!("hello {}", "word ".repeat(150))))
            .await
        {
            Ok(reply) => {
                // A turn may stop midway when the log fills: then the next
                // message is refused.
                drop(
                    within(Duration::from_secs(2), async {
                        until(&handle, ended(reply.turn_id)).await;
                    })
                    .await,
                );
            }
            Err(error) => {
                full = Some(error);
                break;
            }
        }
    }
    assert!(
        matches!(
            full,
            Some(CommandError::Rejected(Rejection::SessionFull { .. }))
        ),
        "the session fills: {full:?}"
    );
    let acked = records(&handle).await;
    let bytes = disk.bytes(session_id()).expect("the log");
    assert!(
        u64::try_from(bytes.len()).expect("small") <= small.log_file_bytes,
        "the file never passes its bound"
    );
    first.world.shutdown().cancel();
    drop(handle);

    let second = boot_with(&disk, Arc::new(MemEnv::default()), small.clone()).await;
    let handle = second
        .registry
        .lookup(session_id())
        .await
        .expect("it boots");
    let recovered = records(&handle).await;
    assert_eq!(
        recovered.get(..acked.len()),
        Some(acked.as_slice()),
        "every acknowledged record replays"
    );
    // The restart's own small records may still fit; the session carries on
    // until an append is refused again.
    let refused = eventually_refused(&handle).await;
    assert!(
        matches!(
            refused,
            Some(CommandError::Rejected(
                Rejection::SessionFull { .. } | Rejection::SessionBusy { .. }
            ))
        ),
        "{refused:?}"
    );
    assert!(
        u64::try_from(disk.bytes(session_id()).expect("the log").len()).expect("small")
            <= small.log_file_bytes,
        "still within its bound"
    );
    fold(&handle).await;
    second.world.shutdown().cancel();
}

/// The first refusal of a new message within two seconds.
async fn eventually_refused(handle: &SessionHandle) -> Option<CommandError> {
    for seed in 100..=120u8 {
        if let Err(error) = handle.send_message(send(seed, "more")).await {
            return Some(error);
        }
        SystemClock.sleep(Millis::new(100)).await;
    }
    None
}
