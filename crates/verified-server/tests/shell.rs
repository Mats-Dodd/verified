//! In-process tests of the shell: the real core, shell, projection, history,
//! tools and log framing, with an in-memory environment and an in-memory
//! disk, and the scripted model served on loopback to the real Anthropic
//! client. No filesystem, and the one socket is bound directly rather than
//! through the shim, so they run the same with and without the `sim`
//! feature.
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test harness code, not the code under test"
)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use verified_domain::id::{ActorId, CallId, IdEntropy, MessageId, TurnId, UnixMillis};
use verified_domain::{Exit, Grant, Limits, LimitsConfig, Millis, ModelFault, Record, TurnOutcome};
use verified_server::daemon::{self, DaemonError, Registry};
use verified_server::env::{
    BashGrant, DirEntry, EditGrant, EntryKind, Env, EnvError, ExecOptions, ExitStatus, Kept,
    Stopped, Written,
};
use verified_server::limits::ServerLimits;
use verified_server::log::{Log, LogError, LogStore, MemDisk, MemLogStore};
use verified_server::payload::{Parsed, ProjectPath};
use verified_server::projection::Projector;
use verified_server::seam::{Clock, OsEntropy, SystemClock, Tasks};
use verified_server::shell::{CommandError, Planted, Seams, SessionHandle, ShellConfig};
use verified_wire::WireLimits;
use verified_wire::chunk::{TurnEndReason, UiChunk};
use verified_wire::command::{
    Approve, Cancel, MessageText, Outcome, Rejection, SendMessage, Verdict,
};
use verified_wire::stream::Offset;
use verified_wire::transcript::Transcript;

use common::{fake_model, within};

/// Files and a shell in memory. `block` commands wait for their cancel.
#[derive(Debug, Default)]
struct MemEnv {
    files: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
    commands: std::sync::Mutex<Vec<(String, String)>>,
    writes: std::sync::Mutex<Vec<String>>,
    /// Writes never return: a crash between dispatch and result.
    stall_writes: bool,
    /// The rules file's text, if there is one.
    rules: std::sync::Mutex<Option<String>>,
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

    /// Writes the rules file (`None`: removes it).
    fn set_rules(&self, text: Option<&str>) {
        *self.rules.lock().expect("unpoisoned") = text.map(str::to_owned);
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
        cancel: &CancellationToken,
    ) -> Result<(ExitStatus, Kept), EnvError> {
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
        let mut kept = Kept::new(options.output_bytes);
        if command.as_str() == "block" {
            cancel.cancelled().await;
            return Ok((ExitStatus::Stopped(Stopped::Cancelled), kept));
        }
        if command.as_str() == "hang" {
            // A run that ignores its cancel and never reports.
            core::future::pending::<()>().await;
        }
        kept.push(format!("ran {}\n", command.as_str()).as_bytes());
        Ok((ExitStatus::Code(0), kept))
    }

    async fn rules_file(&self, _limit: u64) -> Result<Option<Vec<u8>>, EnvError> {
        Ok(self
            .rules
            .lock()
            .expect("unpoisoned")
            .clone()
            .map(String::into_bytes))
    }
}

struct Daemon {
    registry: Registry,
    tasks: Tasks,
    _fatal: mpsc::Receiver<DaemonError>,
}

fn limits() -> ServerLimits {
    let mut limits = ServerLimits::v0();
    limits.core = Limits::new(LimitsConfig {
        retry_schedule: vec![Millis::new(1), Millis::new(1)],
        ..LimitsConfig::v0()
    });
    limits
}

async fn boot(disk: &MemDisk, env: Arc<dyn Env>) -> Daemon {
    boot_with(disk, env, limits()).await
}

async fn boot_with(disk: &MemDisk, env: Arc<dyn Env>, limits: ServerLimits) -> Daemon {
    let store = Arc::new(MemLogStore::new(
        disk.clone(),
        limits.log_line_bytes,
        limits.log_file_bytes,
    ));
    boot_on(store, env, limits, Planted::None).await
}

async fn boot_on(
    store: Arc<dyn LogStore>,
    env: Arc<dyn Env>,
    limits: ServerLimits,
    planted: Planted,
) -> Daemon {
    let tasks = Tasks::new();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let seams = Seams {
        model: Arc::new(fake_model(&limits, &clock, &tasks).await),
        env,
        clock,
        entropy: Arc::new(OsEntropy),
        tasks: tasks.clone(),
    };
    let config = Arc::new(ShellConfig {
        limits: limits.clone(),
        model: "fake".to_owned(),
        system: "You are a test.".to_owned(),
        planted,
    });
    let (fatal, failed) = mpsc::channel(1);
    let registry = daemon::boot(store, seams, config, fatal)
        .await
        .expect("the daemon boots");
    Daemon {
        registry,
        tasks,
        _fatal: failed,
    }
}

fn session_id() -> ActorId {
    ActorId::from_parts(
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
        let read = handle.read(from).await.expect("the session runs");
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
    let committed = handle.committed();
    loop {
        let seen = committed.get();
        let chunks = all_chunks(handle).await;
        if ready(&chunks) {
            return chunks;
        }
        within(Duration::from_secs(20), committed.past(seen))
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
    daemon.tasks.shutdown().cancel();
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
    daemon.tasks.shutdown().cancel();
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
    daemon.tasks.shutdown().cancel();
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
        reply_text(&transcript, 0).contains("Error: Not started: the user denied this tool call."),
        "the model heard the refusal: {}",
        reply_text(&transcript, 0)
    );
    daemon.tasks.shutdown().cancel();
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
    daemon.tasks.shutdown().cancel();
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
    let other = TurnId(999_999);
    let nothing = handle.cancel(Cancel { turn_id: other }).await;
    assert!(
        matches!(
            nothing,
            Err(CommandError::Rejected(Rejection::TurnNotRunning { turn_id })) if turn_id == other
        ),
        "a cancel names its turn; another turn is not running: {nothing:?}"
    );
    daemon.tasks.shutdown().cancel();
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
                outcome: TurnOutcome::Failed(verified_domain::Failure::Model(
                    ModelFault::Overloaded
                )),
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
    daemon.tasks.shutdown().cancel();
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
    daemon.tasks.shutdown().cancel();
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
    first.tasks.shutdown().cancel();
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
                exit: Exit::Unknown,
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
    second.tasks.shutdown().cancel();
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
    first.tasks.shutdown().cancel();
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
    second.tasks.shutdown().cancel();
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
    let tasks = Tasks::new();
    let seams = Seams {
        model: Arc::new(fake_model(&limits(), &clock, &tasks).await),
        env: Arc::new(MemEnv::default()),
        clock,
        entropy: Arc::new(OsEntropy),
        tasks,
    };
    let config = Arc::new(ShellConfig {
        limits: limits(),
        model: "fake".to_owned(),
        system: String::new(),
        planted: Planted::None,
    });
    let refused = daemon::boot(
        Arc::new(MemLogStore::new(disk.clone(), 1 << 20, 1 << 30)),
        seams,
        config,
        fatal,
    )
    .await;
    assert!(
        matches!(refused, Err(DaemonError::Log(LogError::Corrupt { .. }))),
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
    first.tasks.shutdown().cancel();
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
                exit: Exit::Unknown,
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
    second.tasks.shutdown().cancel();
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
    daemon.tasks.shutdown().cancel();
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
            exit: Exit::Unknown,
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
    daemon.tasks.shutdown().cancel();
}

/// Bounds small enough to fill a log in a test.
fn small_limits() -> ServerLimits {
    let mut small = limits();
    small.wire.max_message_text_bytes = 1_000;
    small.tool_output_bytes = 1_000;
    small.log_line_bytes = 16usize << 10u32;
    small.log_file_bytes = 48u64 << 10u32;
    small
}

/// A log that reaches its file bound refuses the append that would pass it:
/// nothing of it is written, the session fails stop (it refuses commands and
/// appends nothing more), and the log still replays to exactly what was
/// acknowledged, including after a restart, which serves it instead of
/// crashing.
#[tokio::test]
async fn a_full_log_stops_taking_records_and_loses_nothing() {
    let small = small_limits();
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
    let again = handle.send_message(send(99, "anyone?")).await;
    assert!(
        matches!(
            again,
            Err(CommandError::Rejected(Rejection::SessionFull { .. }))
        ),
        "fail-stop: every later command is refused: {again:?}"
    );
    assert_eq!(
        disk.bytes(session_id()),
        Some(bytes.clone()),
        "fail-stop: nothing more is appended"
    );
    assert_eq!(records(&handle).await, acked, "nor applied");
    assert!(
        u64::try_from(bytes.len()).expect("small") <= small.log_file_bytes,
        "the file never passes its bound"
    );
    first.tasks.shutdown().cancel();
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
            Some(CommandError::Rejected(Rejection::SessionFull { .. }))
        ),
        "{refused:?}"
    );
    assert!(
        u64::try_from(disk.bytes(session_id()).expect("the log").len()).expect("small")
            <= small.log_file_bytes,
        "still within its bound"
    );
    fold(&handle).await;
    second.tasks.shutdown().cancel();
}

/// Both the registry and a session see shutdown before a command already
/// queued for them: each takes its next input only while the daemon runs.
#[tokio::test]
async fn shutdown_wins_over_a_queued_command() {
    let disk = MemDisk::new();
    let daemon = boot(&disk, Arc::new(MemEnv::default())).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let before = disk.bytes(session_id()).expect("the log");
    let other = ActorId::from_parts(
        UnixMillis::new(1_700_000_000_000).expect("a time"),
        &[8; 10],
    );
    daemon.tasks.shutdown().cancel();
    // Both commands are queued before either task runs again.
    let (created, reply) = tokio::join!(
        daemon.registry.create(other),
        handle.send_message(send(1, "after shutdown")),
    );
    assert!(
        reply.is_err(),
        "a stopping daemon takes no command: {reply:?}"
    );
    assert_eq!(
        disk.bytes(session_id()),
        Some(before),
        "a stopping daemon appends nothing"
    );
    assert!(
        created.is_err(),
        "a stopping daemon creates no session: {created:?}"
    );
    assert_eq!(disk.bytes(other), None, "a stopping daemon makes no log");
}

/// A crash mid-turn on a log too full to carry the turn on: the restart's
/// resume is refused, and the session fails stop instead of crash-looping.
/// It serves its log as it is, refuses commands and appends nothing.
#[tokio::test]
async fn a_restart_on_a_full_log_fails_stop() {
    let roomy = ServerLimits {
        log_file_bytes: 1 << 20,
        ..small_limits()
    };
    let disk = MemDisk::new();
    let first = boot_with(&disk, Arc::new(MemEnv::default()), roomy).await;
    let (_, handle) = first.registry.create(session_id()).await.expect("created");
    let length = || disk.bytes(session_id()).map_or(0, |bytes| bytes.len());
    let mut seed = 1u8;
    while length() < 40usize << 10u32 {
        let reply = handle
            .send_message(send(seed, &"x".repeat(990)))
            .await
            .expect("accepted");
        until(&handle, ended(reply.turn_id)).await;
        seed = seed.saturating_add(1);
    }
    let outputs = || {
        disk.bytes(session_id()).map_or(0, |bytes| {
            String::from_utf8_lossy(&bytes)
                .matches("\"ModelOutput\"")
                .count()
        })
    };
    let before = outputs();
    let slow = handle
        .send_message(send(seed, "slow and steady"))
        .await
        .expect("accepted");
    assert!(
        eventually(|| outputs() > before).await,
        "the slow reply streams"
    );
    first.tasks.shutdown().cancel();
    drop(handle);

    // No room for even the restart's first record.
    let full = disk.bytes(session_id()).expect("the log");
    let tight = ServerLimits {
        log_file_bytes: u64::try_from(full.len()).expect("small"),
        ..small_limits()
    };
    let second = boot_with(&disk, Arc::new(MemEnv::default()), tight).await;
    let handle = second
        .registry
        .lookup(session_id())
        .await
        .expect("it boots, and does not crash");
    let log = records(&handle).await;
    assert!(
        log.iter()
            .any(|record| matches!(record, Record::TurnStarted { turn } if *turn == slow.turn_id)),
        "the turn the crash left running is served as the log says"
    );
    let refused = handle.send_message(send(99, "more")).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Rejected(Rejection::SessionFull { .. }))
        ),
        "{refused:?}"
    );
    let cancelled = handle
        .cancel(Cancel {
            turn_id: slow.turn_id,
        })
        .await;
    assert!(
        matches!(
            cancelled,
            Err(CommandError::Rejected(Rejection::SessionFull { .. }))
        ),
        "{cancelled:?}"
    );
    assert_eq!(disk.bytes(session_id()), Some(full), "nothing appended");
    fold(&handle).await;
    second.tasks.shutdown().cancel();
}

/// The first refusal of a new message within two seconds, other than busy
/// (the turn a restart carried on may still be running).
async fn eventually_refused(handle: &SessionHandle) -> Option<CommandError> {
    for seed in 100..=120u8 {
        match handle.send_message(send(seed, "more")).await {
            Err(CommandError::Rejected(Rejection::SessionBusy { .. })) | Ok(_) => {}
            Err(error) => return Some(error),
        }
        SystemClock.sleep(Millis::new(100)).await;
    }
    None
}

/// What a [`Watched`] log does with a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Append,
    /// Refuse it, as a full disk would.
    Refuse,
    /// Never return: the daemon is stuck there until it crashes.
    Stall,
    /// Append it after a while.
    Slow,
}

type Rule = Arc<dyn Fn(ActorId, &[Record<Parsed>]) -> Act + Send + Sync>;

/// One durable group, in the order groups became durable across logs.
type Order = Arc<std::sync::Mutex<Vec<(ActorId, u64, Vec<Record<Parsed>>)>>>;

/// Logs on a [`MemDisk`] whose groups `rule` may refuse, stall or slow,
/// recording the order every group became durable in, across actors.
struct Watched {
    inner: MemLogStore,
    rule: Rule,
    order: Order,
}

impl core::fmt::Debug for Watched {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("Watched").finish_non_exhaustive()
    }
}

impl Watched {
    fn new(disk: &MemDisk, limits: &ServerLimits, rule: Rule) -> (Arc<Self>, Order) {
        let order = Order::default();
        let store = Arc::new(Self {
            inner: MemLogStore::new(disk.clone(), limits.log_line_bytes, limits.log_file_bytes),
            rule,
            order: Arc::clone(&order),
        });
        (store, order)
    }

    fn wrap(&self, actor: ActorId, log: Box<dyn Log>) -> Box<dyn Log> {
        Box::new(WatchedLog {
            actor,
            inner: log,
            rule: Arc::clone(&self.rule),
            order: Arc::clone(&self.order),
        })
    }
}

struct WatchedLog {
    actor: ActorId,
    inner: Box<dyn Log>,
    rule: Rule,
    order: Order,
}

impl core::fmt::Debug for WatchedLog {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("WatchedLog").finish_non_exhaustive()
    }
}

#[async_trait]
impl Log for WatchedLog {
    fn next_seq(&self) -> u64 {
        self.inner.next_seq()
    }

    async fn append(&mut self, records: &[Record<Parsed>]) -> Result<(), LogError> {
        match (self.rule)(self.actor, records) {
            Act::Append => {}
            Act::Refuse => {
                return Err(LogError::Full {
                    path: "watched".to_owned(),
                    length: 0,
                    limit: 0,
                });
            }
            Act::Stall => core::future::pending::<()>().await,
            Act::Slow => SystemClock.sleep(Millis::new(200)).await,
        }
        let first = self.inner.next_seq();
        self.inner.append(records).await?;
        self.order
            .lock()
            .expect("unpoisoned")
            .push((self.actor, first, records.to_vec()));
        Ok(())
    }
}

#[async_trait]
impl LogStore for Watched {
    async fn list(&self) -> Result<Vec<ActorId>, LogError> {
        self.inner.list().await
    }

    async fn create(&self, session: ActorId) -> Result<Box<dyn Log>, LogError> {
        Ok(self.wrap(session, self.inner.create(session).await?))
    }

    async fn open(
        &self,
        session: ActorId,
    ) -> Result<(Box<dyn Log>, Vec<Record<Parsed>>), LogError> {
        let (log, records) = self.inner.open(session).await?;
        Ok((self.wrap(session, log), records))
    }
}

/// L3's fail-stop, checked: an approved `bash` whose `Dispatched` the log
/// refuses. What broke it, if anything: the approval acknowledged, the
/// command run, or anything appended after the refusal.
async fn fail_stop(planted: Planted) -> Result<(), String> {
    let disk = MemDisk::new();
    let env = Arc::new(MemEnv::default());
    let limits = limits();
    // The disk fills exactly where the next act would.
    let (store, _) = Watched::new(
        &disk,
        &limits,
        Arc::new(|_, records: &[Record<Parsed>]| {
            if records
                .iter()
                .any(|record| matches!(record, Record::Dispatched { .. }))
            {
                Act::Refuse
            } else {
                Act::Append
            }
        }),
    );
    let daemon = boot_on(store, Arc::<MemEnv>::clone(&env), limits, planted).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    handle
        .send_message(send(1, "bash echo hi"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    let call_id = approval_requested(&chunks).expect("asked");
    let before = disk.bytes(session_id());
    let approved = handle
        .approve(Approve {
            call_id,
            verdict: Verdict::Grant,
        })
        .await;
    let more = handle.send_message(send(2, "still there?")).await;
    SystemClock.sleep(Millis::new(300)).await;
    let mut broken = Vec::new();
    for answer in [approved.map(drop), more.map(drop)] {
        if !matches!(
            answer,
            Err(CommandError::Rejected(Rejection::SessionFull { .. }))
        ) {
            broken.push(format!("answered {answer:?}"));
        }
    }
    if !env.commands().is_empty() {
        broken.push(format!("acted: {:?}", env.commands()));
    }
    if disk.bytes(session_id()) != before {
        broken.push("appended after the refusal".to_owned());
    }
    daemon.tasks.shutdown().cancel();
    if broken.is_empty() {
        Ok(())
    } else {
        Err(broken.join("; "))
    }
}

/// Storage is the world: a refused append halts the session. Nothing acts
/// without its durable record, nothing is acknowledged, and nothing more
/// is appended (L3).
#[tokio::test]
async fn a_refused_append_fails_stop() {
    fail_stop(Planted::None)
        .await
        .expect("nothing acts or is acknowledged after a refused append");
}

/// The planted bug for L3's fail-stop: a shell that starts a decision's
/// effects although the log refused its records. The check must catch it.
#[cfg(feature = "sim")]
#[tokio::test]
async fn acting_after_a_refused_append_is_caught() {
    let broken = fail_stop(Planted::ActAfterRefusal)
        .await
        .expect_err("the check catches a shell that acts after a refused append");
    assert!(broken.contains("acted"), "{broken}");
}

/// The rules file grants in advance (A2): a rule that fits runs the call
/// without asking, under a grant naming the rule and the `RulesLoaded` that
/// holds it, with the file's text in the log. The file is read again before
/// each message: once it is gone, the same command asks.
#[tokio::test]
async fn the_rules_file_grants_and_is_read_before_each_message() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some("[[allow]]\ntool = \"bash\"\nprefix = \"echo\"\n"));
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let reply = handle
        .send_message(send(1, "bash echo granted"))
        .await
        .expect("accepted");
    until(&handle, ended(reply.turn_id)).await;
    assert_eq!(
        env.commands().first().map(|(command, _)| command.clone()),
        Some("echo granted".to_owned()),
        "ran without asking"
    );
    let log = records(&handle).await;
    let loaded = log
        .iter()
        .enumerate()
        .find_map(|(seq, record)| {
            if let Record::RulesLoaded { file, .. } = record {
                Some((seq, file.clone()))
            } else {
                None
            }
        })
        .expect("the rules are in the log");
    assert!(
        loaded
            .1
            .is_some_and(|file| file.body.contains("prefix = \"echo\"")),
        "with the file's text"
    );
    assert!(
        log.iter().any(|record| matches!(
            record,
            Record::Approved {
                by: Grant::Rule { rule: 0, rules },
                ..
            } if *rules == u64::try_from(loaded.0).expect("small")
        )),
        "granted by rule 0 of that version: {log:?}"
    );
    assert!(
        !log.iter().any(|record| matches!(
            record,
            Record::Approved {
                by: Grant::User,
                ..
            }
        )),
        "nobody was asked"
    );

    env.set_rules(None);
    let second = handle
        .send_message(send(2, "bash echo asked"))
        .await
        .expect("accepted");
    let chunks = until(&handle, |chunks| approval_requested(chunks).is_some()).await;
    assert!(
        approval_requested(&chunks).is_some(),
        "the same command asks now"
    );
    let log = records(&handle).await;
    assert_eq!(
        log.iter()
            .filter(|record| matches!(record, Record::RulesLoaded { .. }))
            .count(),
        2,
        "the change is recorded once, before the message it applies to"
    );
    assert_eq!(env.commands().len(), 1, "the second has not run");
    handle
        .cancel(Cancel {
            turn_id: second.turn_id,
        })
        .await
        .expect("cancelled");
    daemon.tasks.shutdown().cancel();
}

/// Waits until `ready` holds for the log, and returns it.
async fn until_log<F>(handle: &SessionHandle, ready: F) -> Vec<Record<Parsed>>
where
    F: Fn(&[Record<Parsed>]) -> bool,
{
    let committed = handle.committed();
    loop {
        let seen = committed.get();
        let log = records(handle).await;
        if ready(&log) {
            return log;
        }
        within(Duration::from_secs(20), committed.past(seen))
            .await
            .expect("the log grows within 20s")
            .expect("the session runs");
    }
}

/// The seq of the first `Dispatched` in `log` after `from`.
fn dispatched_after(log: &[Record<Parsed>], from: usize) -> Option<u64> {
    log.iter()
        .enumerate()
        .skip(from)
        .find(|(_, record)| matches!(record, Record::Dispatched { .. }))
        .and_then(|(seq, _)| u64::try_from(seq).ok())
}

/// Whether a step carried task `task`'s end: its notification was read.
fn carried(log: &[Record<Parsed>], task: u64) -> bool {
    log.iter().any(|record| {
        matches!(record, Record::StepStarted { carries, .. }
            if carries.contains(&verified_domain::EnvelopeId::Task(verified_domain::TaskId(task))))
    })
}

/// The end of task `task`, once it has one.
fn task_end(log: &[Record<Parsed>], task: u64) -> Option<(Exit, Option<String>)> {
    log.iter().find_map(|record| {
        if let Record::TaskEnded {
            task: ended,
            exit,
            output,
        } = record
            && ended.0 == task
        {
            Some((
                *exit,
                output.as_ref().map(|output| output.body.text.clone()),
            ))
        } else {
            None
        }
    })
}

/// A background command resolves its call at its durable start (no turn
/// waits for it, B1), runs on as a task, and its end reaches the actor as
/// a notification a later model call carries (H3).
#[tokio::test]
async fn a_background_command_resolves_at_once_and_notifies_when_it_ends() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some("[[allow]]\ntool = \"bash\"\n"));
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    handle
        .send_message(send(1, "bg echo later"))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| {
        dispatched_after(log, 0).is_some_and(|task| carried(log, task))
    })
    .await;
    let task = dispatched_after(&log, 0).expect("dispatched");
    let at = usize::try_from(task).expect("small");
    assert!(
        matches!(
            log.get(at.saturating_add(1)),
            Some(Record::ToolResult {
                exit: Exit::Ok,
                output: None,
                ..
            })
        ),
        "the call resolved in the decision that started it: {log:?}"
    );
    assert_eq!(
        task_end(&log, task),
        Some((Exit::Ok, Some("ran echo later\n[exit status 0]".to_owned()))),
        "the task ended once, with what it reported"
    );
    assert_eq!(env.commands().len(), 1, "it ran once");
    daemon.tasks.shutdown().cancel();
}

/// `wake` is a timer task: it ends `Ok` at its deadline, and its end
/// starts a turn that carries it.
#[tokio::test]
async fn a_timer_wakes_its_actor() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some("[[allow]]\ntool = \"wake\"\n"));
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    handle
        .send_message(send(1, "wake 50"))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| {
        dispatched_after(log, 0).is_some_and(|task| carried(log, task))
    })
    .await;
    let task = dispatched_after(&log, 0).expect("dispatched");
    assert_eq!(
        task_end(&log, task),
        Some((Exit::Ok, Some("woke".to_owned())))
    );
    daemon.tasks.shutdown().cancel();
}

/// `tasks` lists the actor's tasks, and `cancel` signals a running one,
/// which then records what came back: a command stopped mid-run is
/// `Unknown` (H2).
#[tokio::test]
async fn tasks_lists_and_cancel_signals_a_running_task() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(
        "[[allow]]\ntool = \"bash\"\n[[allow]]\ntool = \"tasks\"\n[[allow]]\ntool = \"cancel\"\n",
    ));
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    let first = handle
        .send_message(send(1, "bg block"))
        .await
        .expect("accepted");
    until(&handle, ended(first.turn_id)).await;
    let task = dispatched_after(&records(&handle).await, 0).expect("dispatched");
    let listed = handle
        .send_message(send(2, "tasks"))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| {
        log.iter().any(
            |record| matches!(record, Record::TurnEnded { turn, .. } if *turn == listed.turn_id),
        )
    })
    .await;
    assert!(
        log.iter().any(|record| matches!(
            record,
            Record::ToolResult { exit: Exit::Ok, output: Some(output), .. }
                if output.body.text == format!("task {task}: bash `block`: running")
        )),
        "the listing names the running task: {log:?}"
    );
    handle
        .send_message(send(3, &format!("cancel {task}")))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| task_end(log, task).is_some()).await;
    assert!(
        matches!(task_end(&log, task), Some((Exit::Unknown, Some(_)))),
        "a command stopped mid-run may have acted: {log:?}"
    );
    daemon.tasks.shutdown().cancel();
}

/// B2, checked: a foreground `bash` and a background one whose runs never
/// report (they ignore their cancel). What broke, if anything: a call or a
/// task not resolved `Unknown` within two seconds of its deadline.
async fn deadlines(planted: Planted) -> Result<(), String> {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some("[[allow]]\ntool = \"bash\"\n"));
    let mut limits = limits();
    limits.exec_timeout = Millis::new(50);
    limits.task_timeout = Millis::new(50);
    limits.exec_drain = Millis::new(10);
    limits.deadline_slack = Millis::new(50);
    let store = Arc::new(MemLogStore::new(
        MemDisk::new(),
        limits.log_line_bytes,
        limits.log_file_bytes,
    ));
    let daemon = boot_on(store, Arc::<MemEnv>::clone(&env), limits, planted).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    handle
        .send_message(send(1, "bash hang ; bg hang"))
        .await
        .expect("accepted");
    let resolved = |log: &[Record<Parsed>]| {
        let foreground = log.iter().any(|record| {
            matches!(
                record,
                Record::ToolResult {
                    exit: Exit::Unknown,
                    output: None,
                    ..
                }
            )
        });
        let background = log.iter().any(|record| {
            matches!(
                record,
                Record::TaskEnded {
                    exit: Exit::Unknown,
                    output: None,
                    ..
                }
            )
        });
        foreground && background
    };
    let waited = within(Duration::from_secs(2), until_log(&handle, resolved)).await;
    let log = records(&handle).await;
    daemon.tasks.shutdown().cancel();
    match waited {
        Ok(_) => Ok(()),
        Err(_) => Err(format!(
            "a run that never reports was waited on past its deadline: {log:?}"
        )),
    }
}

/// Every in-flight effect resolves by its deadline (B2): a run that never
/// reports is `Unknown` once v stops waiting.
#[tokio::test]
async fn a_run_that_never_reports_resolves_unknown_at_its_deadline() {
    deadlines(Planted::None)
        .await
        .expect("every run resolves by its deadline");
}

/// The planted bug for B2: no deadline is armed. The check must catch the
/// wait.
#[cfg(feature = "sim")]
#[tokio::test]
async fn waiting_on_a_run_that_never_reports_is_caught() {
    let broken = deadlines(Planted::NoDeadline)
        .await
        .expect_err("the check catches a run waited on for good");
    assert!(broken.contains("past its deadline"), "{broken}");
}

/// Rules that let every actor spawn and send without asking.
const MAIL: &str =
    "[[allow]]\ntool = \"spawn\"\n[[allow]]\ntool = \"send\"\n[[allow]]\ntool = \"bash\"\n";

fn other_id() -> ActorId {
    ActorId::from_parts(
        UnixMillis::new(1_700_000_000_000).expect("a time"),
        &[8; 10],
    )
}

/// The envelopes `log` holds from `from`.
fn delivered_from(log: &[Record<Parsed>], from: ActorId) -> Vec<verified_domain::EnvelopeId> {
    log.iter()
        .filter_map(|record| {
            if let Record::Delivered { envelope } = record
                && envelope.from == verified_domain::Address::Actor(from)
            {
                Some(envelope.id)
            } else {
                None
            }
        })
        .collect()
}

/// The first `ToolResult` in `log` whose output starts with `prefix`.
fn result_saying(log: &[Record<Parsed>], prefix: &str) -> Option<Exit> {
    log.iter().find_map(|record| {
        if let Record::ToolResult {
            exit,
            output: Some(output),
            ..
        } = record
            && output.body.text.starts_with(prefix)
        {
            Some(*exit)
        } else {
            None
        }
    })
}

/// Actors are sessions: a spawned child is a new actor with its own log,
/// born as its parent named it (one deeper, its parent's id kept for
/// display), whose prompt lands in its mailbox; what it sends its parent
/// lands in the parent's mailbox once, and starts a turn there.
#[tokio::test]
async fn a_spawned_child_runs_and_answers_its_parent() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(MAIL));
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let parent = session_id();
    let (_, handle) = daemon.registry.create(parent).await.expect("created");
    handle
        .send_message(send(1, &format!("spawn helper send {parent} hello parent")))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| {
        !delivered_from(log, other(log)).is_empty()
            && log
                .iter()
                .filter(|record| matches!(record, Record::TurnEnded { .. }))
                .count()
                >= 2
    })
    .await;
    let child = other(&log);
    assert_eq!(
        result_saying(&log, "spawned helper at"),
        Some(Exit::Ok),
        "the spawn reports where the child is"
    );
    assert_eq!(
        delivered_from(&log, child).len(),
        1,
        "the answer landed once"
    );
    let listed = daemon.registry.list().await;
    let born = listed
        .iter()
        .find(|listed| listed.actor == child)
        .map(|listed| listed.born.clone())
        .expect("the registry hosts the child");
    assert_eq!(born.name.as_str(), "helper");
    assert_eq!(born.parent, Some(parent));
    assert_eq!(born.depth, 1);
    let child_log = records(&daemon.registry.lookup(child).await.expect("hosted")).await;
    assert_eq!(
        delivered_from(&child_log, parent).len(),
        1,
        "the prompt landed once in the child's log"
    );
    assert_eq!(
        result_saying(&child_log, &format!("delivered to {parent}")),
        Some(Exit::Ok)
    );
    daemon.tasks.shutdown().cancel();
}

/// The child a log's spawn made: the actor of its first spawn proposal.
fn other(log: &[Record<Parsed>]) -> ActorId {
    log.iter()
        .find_map(|record| {
            if let Record::StepFinished { calls, .. } = record {
                calls.iter().find_map(|call| match &call.request {
                    verified_domain::Request::Run {
                        args: verified_server::payload::ToolArgs::Spawn { child, .. },
                    } => Some(*child),
                    verified_domain::Request::Run { .. }
                    | verified_domain::Request::Invalid { .. } => None,
                })
            } else {
                None
            }
        })
        .unwrap_or_else(other_id)
}

/// Your stop revokes what the actor holds and stops every actor it
/// spawned: the cascade. A stop is `Keyed`: a repeat is `Exists`.
#[tokio::test]
async fn stopping_an_actor_stops_its_children() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(MAIL));
    let daemon = boot(&MemDisk::new(), Arc::<MemEnv>::clone(&env)).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    handle
        .send_message(send(1, "spawn helper bg block"))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| {
        result_saying(log, "spawned helper").is_some()
    })
    .await;
    let child = other(&log);
    let child_handle = daemon.registry.lookup(child).await.expect("hosted");
    until_log(&child_handle, |log| {
        log.iter()
            .any(|record| matches!(record, Record::Dispatched { .. }))
    })
    .await;
    let stop = message_id(50);
    assert_eq!(
        handle.stop(stop).await.expect("answered"),
        verified_core::Outcome::Accepted
    );
    assert_eq!(
        handle.stop(stop).await.expect("answered"),
        verified_core::Outcome::Exists,
        "a repeated stop is a repeat"
    );
    let log = until_log(&handle, |log| {
        result_saying(log, "stopped").is_some()
            || log
                .iter()
                .any(|record| matches!(record, Record::TaskEnded { exit: Exit::Ok, .. }))
    })
    .await;
    assert!(
        log.iter().any(
            |record| matches!(record, Record::StopSent { child: stopped } if *stopped == child)
        ),
        "the parent stops its child"
    );
    let child_log = until_log(&child_handle, |log| {
        log.iter()
            .any(|record| matches!(record, Record::TaskEnded { .. }))
    })
    .await;
    assert!(
        child_log.iter().any(|record| matches!(
            record,
            Record::Delivered { envelope } if envelope.body == verified_domain::Body::Stop
                && envelope.from == verified_domain::Address::Runtime
        )),
        "the child holds a stop from v"
    );
    assert!(
        child_log.iter().any(|record| matches!(
            record,
            Record::TaskEnded {
                exit: Exit::Unknown,
                ..
            }
        )),
        "the child's running command was signalled and may have acted: {child_log:?}"
    );
    daemon.tasks.shutdown().cancel();
}

/// The actor cap is the registry's (B3): a spawn past it does not start.
#[tokio::test]
async fn a_spawn_past_the_actor_limit_does_not_start() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(MAIL));
    let mut limits = limits();
    limits.sessions = 1;
    let daemon = boot_with(&MemDisk::new(), Arc::<MemEnv>::clone(&env), limits).await;
    let (_, handle) = daemon.registry.create(session_id()).await.expect("created");
    handle
        .send_message(send(1, "spawn helper bash echo never"))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| result_saying(log, "not spawned").is_some()).await;
    assert_eq!(
        result_saying(&log, "not spawned"),
        Some(Exit::NotStarted(verified_domain::Unstarted::Limit))
    );
    assert_eq!(daemon.registry.list().await.len(), 1, "no second actor");
    daemon.tasks.shutdown().cancel();
}

/// O2 between logs, checked: actor A sends to actor B, and the daemon
/// crashes after B's `Delivered` is durable and before A's result is. The
/// restart runs the send again under its key. What broke, if anything:
/// B holding the envelope other than once, or A's send not resolved `Ok`.
async fn exactly_once(planted: Planted) -> Result<(), String> {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(MAIL));
    let disk = MemDisk::new();
    let limits = limits();
    let (from, to) = (session_id(), other_id());
    // A's result never becomes durable: the crash comes first.
    let (store, _) = Watched::new(
        &disk,
        &limits,
        Arc::new(move |actor, records: &[Record<Parsed>]| {
            let result = records.iter().any(|record| {
                matches!(record, Record::ToolResult { output: Some(output), .. }
                    if output.body.text.starts_with("delivered to"))
            });
            if actor == from && result {
                Act::Stall
            } else {
                Act::Append
            }
        }),
    );
    let first = boot_on(store, Arc::<MemEnv>::clone(&env), limits.clone(), planted).await;
    let (_, sender) = first.registry.create(from).await.expect("created");
    let (_, recipient) = first.registry.create(to).await.expect("created");
    sender
        .send_message(send(1, &format!("send {to} hello b")))
        .await
        .expect("accepted");
    until_log(&recipient, |log| !delivered_from(log, from).is_empty()).await;
    first.tasks.shutdown().cancel();
    drop((sender, recipient));

    let (store, _) = Watched::new(
        &disk,
        &limits,
        Arc::new(|_, _: &[Record<Parsed>]| Act::Append),
    );
    let second = boot_on(store, Arc::<MemEnv>::clone(&env), limits, planted).await;
    let sender = second.registry.lookup(from).await.expect("recovered");
    let recipient = second.registry.lookup(to).await.expect("recovered");
    let sent = within(
        Duration::from_secs(5),
        until_log(&sender, |log| {
            result_saying(log, &format!("delivered to {to}")).is_some()
        }),
    )
    .await;
    let held = delivered_from(&records(&recipient).await, from);
    second.tasks.shutdown().cancel();
    let mut broken = Vec::new();
    if held.len() != 1 {
        broken.push(format!(
            "the recipient holds the envelope {} times: {held:?}",
            held.len()
        ));
    }
    match sent {
        Ok(log) if result_saying(&log, "delivered to") == Some(Exit::Ok) => {}
        Ok(log) => broken.push(format!("the send did not resolve Ok: {log:?}")),
        Err(_) => broken.push("the send never resolved".to_owned()),
    }
    if broken.is_empty() {
        Ok(())
    } else {
        Err(broken.join("; "))
    }
}

/// A `Keyed` send takes effect exactly once across a crash: the rerun
/// carries the same envelope id, and the recipient takes it once (O2).
#[tokio::test]
async fn a_send_lands_exactly_once_across_a_crash() {
    exactly_once(Planted::None)
        .await
        .expect("the envelope lands once and the send resolves Ok");
}

/// The planted bug for O2: a rerun mints a fresh envelope id.
#[cfg(feature = "sim")]
#[tokio::test]
async fn a_fresh_key_after_a_crash_delivers_twice() {
    let broken = exactly_once(Planted::FreshKeyOnResume)
        .await
        .expect_err("the check catches a second delivery");
    assert!(broken.contains("2 times"), "{broken}");
}

/// A1 and L3 between logs, checked: A sends to B, A's `Dispatched` is
/// slow to become durable, and B's log refuses the envelope (full) if
/// `full`. What broke, if anything: B's `Delivered` durable before A's
/// `Dispatched`, or A recording `Ok` for an envelope B does not hold.
async fn delivery_order(planted: Planted, full: bool) -> Result<(), String> {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(MAIL));
    let limits = limits();
    let (from, to) = (session_id(), other_id());
    let (store, order) = Watched::new(
        &MemDisk::new(),
        &limits,
        Arc::new(move |actor, records: &[Record<Parsed>]| {
            let dispatches = records
                .iter()
                .any(|record| matches!(record, Record::Dispatched { .. }));
            let mail = records
                .iter()
                .any(|record| matches!(record, Record::Delivered { envelope } if envelope.from == verified_domain::Address::Actor(from)));
            match (actor == from && dispatches, actor == to && mail && full) {
                (true, _) => Act::Slow,
                (false, true) => Act::Refuse,
                (false, false) => Act::Append,
            }
        }),
    );
    let daemon = boot_on(store, Arc::<MemEnv>::clone(&env), limits, planted).await;
    let (_, sender) = daemon.registry.create(from).await.expect("created");
    let (_, recipient) = daemon.registry.create(to).await.expect("created");
    sender
        .send_message(send(1, &format!("send {to} hello b")))
        .await
        .expect("accepted");
    let log = until_log(&sender, |log| {
        result_saying(log, "delivered to").is_some()
            || result_saying(log, "not delivered").is_some()
    })
    .await;
    let held = delivered_from(&records(&recipient).await, from);
    daemon.tasks.shutdown().cancel();
    let mut broken = Vec::new();
    if result_saying(&log, "delivered to") == Some(Exit::Ok) && held.is_empty() {
        broken
            .push("the sender recorded Ok for an envelope the recipient does not hold".to_owned());
    }
    let order = order.lock().expect("unpoisoned").clone();
    let durable = |actor: ActorId, seq: u64| {
        order.iter().position(|(at, first, records)| {
            *at == actor
                && (*first..first.saturating_add(u64::try_from(records.len()).unwrap_or(0)))
                    .contains(&seq)
        })
    };
    for (at, (actor, _, records)) in order.iter().enumerate() {
        for record in records {
            if let Record::Delivered { envelope } = record
                && let verified_domain::EnvelopeId::Minted { actor: from, seq } = envelope.id
                && *actor == to
                && durable(from, seq).is_none_or(|dispatched| dispatched > at)
            {
                broken.push(format!(
                    "delivered before its Dispatched (seq {seq}) was durable"
                ));
            }
        }
    }
    if broken.is_empty() {
        Ok(())
    } else {
        Err(broken.join("; "))
    }
}

/// A delivery starts only after its `Dispatched` is durable (A1), and an
/// `Ok` send is in its recipient's log (L3: an ack means durable).
#[tokio::test]
async fn a_delivery_waits_for_its_dispatch_and_its_recipients_append() {
    delivery_order(Planted::None, false)
        .await
        .expect("delivered after the dispatch");
    delivery_order(Planted::None, true)
        .await
        .expect("a refused delivery is not Ok");
}

/// The planted bug for A1 between logs: a send delivered before its
/// `Dispatched` is durable.
#[cfg(feature = "sim")]
#[tokio::test]
async fn delivering_a_send_before_its_dispatch_is_durable_is_caught() {
    let broken = delivery_order(Planted::DeliverBeforePersist, false)
        .await
        .expect_err("the check catches the early delivery");
    assert!(broken.contains("before its Dispatched"), "{broken}");
}

/// The planted bug for L3 between logs: a recipient that acknowledges a
/// delivery before its append is durable (here the append is refused).
#[cfg(feature = "sim")]
#[tokio::test]
async fn acknowledging_before_the_append_loses_the_envelope() {
    let broken = delivery_order(Planted::AckBeforeAppend, true)
        .await
        .expect_err("the check catches the lost envelope");
    assert!(broken.contains("does not hold"), "{broken}");
}

/// A crash can tear an actor's first group, its birth with it. At boot an
/// unborn log is a root's, born again as the user made it, unless a
/// spawn in some log names it: then it is a child, born when its parent's
/// `Keyed` spawn runs again, as its parent named it (never as a root).
#[tokio::test]
async fn a_birth_a_crash_tore_is_recorded_again_as_it_was_made() {
    let env = Arc::new(MemEnv::default());
    env.set_rules(Some(MAIL));
    let disk = MemDisk::new();
    let limits = limits();
    let parent = session_id();
    // Every first group of an actor other than the parent never lands.
    let (store, _) = Watched::new(
        &disk,
        &limits,
        Arc::new(move |actor, records: &[Record<Parsed>]| {
            let birth = records
                .iter()
                .any(|record| matches!(record, Record::Born { .. }));
            if actor != parent && birth {
                Act::Stall
            } else {
                Act::Append
            }
        }),
    );
    let first = boot_on(
        store,
        Arc::<MemEnv>::clone(&env),
        limits.clone(),
        Planted::None,
    )
    .await;
    let (_, handle) = first.registry.create(parent).await.expect("created");
    let (_, torn_root) = first.registry.create(other_id()).await.expect("created");
    drop(torn_root);
    handle
        .send_message(send(1, "spawn helper bash echo born"))
        .await
        .expect("accepted");
    let log = until_log(&handle, |log| {
        log.iter()
            .any(|record| matches!(record, Record::Dispatched { .. }))
    })
    .await;
    let child = other(&log);
    first.tasks.shutdown().cancel();
    drop(handle);

    let second = boot(&disk, Arc::<MemEnv>::clone(&env)).await;
    let root = second.registry.lookup(other_id()).await.expect("recovered");
    let root_log = until_log(&root, |log| {
        log.iter()
            .any(|record| matches!(record, Record::Born { .. }))
    })
    .await;
    assert!(
        root_log.iter().any(|record| matches!(
            record,
            Record::Born { born } if born.name.as_str() == "main" && born.depth == 0
        )),
        "a torn root is born again as a root"
    );
    let child_handle = second.registry.lookup(child).await.expect("recovered");
    let child_log = until_log(&child_handle, |log| {
        log.iter()
            .any(|record| matches!(record, Record::Born { .. }))
    })
    .await;
    assert!(
        child_log.iter().any(|record| matches!(
            record,
            Record::Born { born } if born.name.as_str() == "helper"
                && born.depth == 1
                && born.parent == Some(parent)
        )),
        "a torn child is born as its parent named it: {child_log:?}"
    );
    second.tasks.shutdown().cancel();
}
