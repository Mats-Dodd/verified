//! The real seams on the real OS: `LocalEnv` with `/bin/sh` and process
//! groups, `FileLog` on disk, and `daemon::run` with its lock and port file.
//! Without the `sim` feature only: with it, the fs shim is turmoil's.
#![cfg(not(feature = "sim"))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;
use verified_core::{Approved, Core, Effect, Event, ModelExit, ToolCall, Verdict};
use verified_domain::{
    CallId, Cause, Epoch, Exit, Fault, Limits, MessageId, Millis, Proposal, Record, Request,
    SessionId, TurnId, UnixMillis,
};
use verified_server::daemon::{self, DaemonError, Layout};
use verified_server::env::{Fingerprint, LocalEnv};
use verified_server::limits::ServerLimits;
use verified_server::log::{FileLog, Log as _};
use verified_server::model::fake::FakeModel;
use verified_server::payload::{Command, Parsed, ProjectPath, ToolArgs, Truncation, measure};
use verified_server::seam::{
    Clock, FileLock, Lock as _, LockError, OsEntropy, ProcessGroups, SystemClock, World,
    entropy_u64,
};
use verified_server::shell::{Seams, ShellConfig};
use verified_server::shim::{fs, net};
use verified_server::tool;

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

async fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "verified-{name}-{}-{:x}",
        std::process::id(),
        entropy_u64(&OsEntropy)
    ));
    fs::create_dir_all(&dir).await.expect("a scratch dir");
    dir
}

fn at() -> UnixMillis {
    UnixMillis::new(1_700_000_000_000).expect("a 48-bit time")
}

/// Drives a core until it mints an approved call for `args`.
fn approved(args: ToolArgs) -> ToolCall<Approved, Parsed> {
    let mut core: Core<Parsed> = Core::new(Limits::v0());
    let started = core.decide(Event::Message {
        message: MessageId::from_parts(at(), &[1; 10]),
        turn: TurnId::from_parts(at(), &[2; 10]),
        text: measure("go".to_owned()),
    });
    for record in &started.records {
        core.apply(record).expect("decided records apply");
    }
    let id = started
        .effects
        .iter()
        .find_map(|effect| {
            if let Effect::CallModel { id, .. } = effect {
                Some(*id)
            } else {
                None
            }
        })
        .expect("a model call");
    let call = CallId::parse("toolu_test").expect("a tool call id");
    let finished = core.decide(Event::ModelDone {
        id,
        result: ModelExit::Finished(vec![Proposal {
            id: call.clone(),
            request: Request::Run { args },
        }]),
    });
    for record in &finished.records {
        core.apply(record).expect("decided records apply");
    }
    let mut effects = finished.effects;
    if !effects
        .iter()
        .any(|effect| matches!(effect, Effect::RunTool { .. }))
    {
        let granted = core.decide(Event::Approve {
            call,
            verdict: Verdict::Approve,
        });
        for record in &granted.records {
            core.apply(record).expect("decided records apply");
        }
        effects = granted.effects;
    }
    effects
        .into_iter()
        .find_map(|effect| {
            if let Effect::RunTool { call, .. } = effect {
                Some(call)
            } else {
                None
            }
        })
        .expect("the core minted an approved call")
}

fn bash(command: &str) -> ToolCall<Approved, Parsed> {
    approved(ToolArgs::Bash {
        command: Command::parse(command.to_owned()).expect("a command"),
    })
}

fn session() -> SessionId {
    SessionId::from_parts(at(), &[3; 10])
}

fn local(root: PathBuf) -> LocalEnv {
    LocalEnv::new(root, Arc::new(SystemClock), ProcessGroups::default())
}

#[tokio::test]
async fn bash_reports_status_output_and_call_id() {
    let root = scratch("bash").await;
    let env = local(root);
    let limits = ServerLimits::v0();
    let (exit, output) = tool::run(
        &env,
        session(),
        &bash("echo id=$VERIFIED_CALL_ID; echo oops >&2; exit 3"),
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(
        exit,
        Exit::Err(Cause::Failed(Fault::Other)),
        "nonzero exit fails: {}",
        output.text
    );
    assert!(output.text.contains("id=toolu_test"), "{}", output.text);
    assert!(output.text.contains("oops"), "stderr is captured");
    assert!(output.text.ends_with("[exit status 3]"), "{}", output.text);
}

#[tokio::test]
async fn cancel_kills_the_whole_process_group() {
    let root = scratch("cancel").await;
    let env = local(root.clone());
    let limits = ServerLimits::v0();
    let cancel = CancellationToken::new();
    let call = bash("sleep 30 & echo started; sleep 30; echo never");
    let stopper = cancel.clone();
    let stop = async move {
        SystemClock.sleep(Millis::new(300)).await;
        stopper.cancel();
    };
    let ran = async { tool::run(&env, session(), &call, &cancel, &limits).await };
    let ((exit, output), ()) = within(Duration::from_secs(10), async { tokio::join!(ran, stop) })
        .await
        .expect("cancel returns promptly: the background sleep died too");
    assert_eq!(exit, Exit::Err(Cause::Interrupted), "interrupted");
    assert!(output.text.contains("started"), "{}", output.text);
    assert!(!output.text.contains("never"), "{}", output.text);
}

#[tokio::test]
async fn timeout_kills_and_output_is_bounded() {
    let root = scratch("timeout").await;
    let env = local(root);
    let mut limits = ServerLimits::v0();
    limits.exec_timeout = Millis::new(300);
    let (exit, output) = tool::run(
        &env,
        session(),
        &bash("sleep 30"),
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(exit, Exit::Err(Cause::Failed(Fault::Other)), "timed out");
    assert!(output.text.contains("timed out"), "{}", output.text);

    limits.tool_output_bytes = 1000;
    let (exit, output) = tool::run(
        &env,
        session(),
        &bash("yes | head -c 200000"),
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(exit, Exit::Ok, "ran");
    assert!(
        matches!(output.truncation, Truncation::Truncated { omitted_bytes } if omitted_bytes == 199_000),
        "{:?}",
        output.truncation
    );
    assert!(output.text.len() < 1200, "bounded: {}", output.text.len());
}

#[tokio::test]
async fn edit_is_compare_and_swap_and_keyed() {
    let root = scratch("edit").await;
    fs::write_synced(&root.join("lib.rs"), b"fn mian() {}\n")
        .await
        .expect("a file");
    let env = local(root.clone());
    let limits = ServerLimits::v0();
    let call = approved(ToolArgs::Edit {
        path: ProjectPath::parse("lib.rs").expect("a path"),
        old: "mian".to_owned(),
        new: "main".to_owned(),
    });
    let (exit, output) =
        tool::run(&env, session(), &call, &CancellationToken::new(), &limits).await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    let text = fs::read(&root.join("lib.rs")).await.expect("read back");
    assert_eq!(text, b"fn main() {}\n", "edited");
    // Rerun after a crash: same call id, new text already in place.
    let (exit, output) =
        tool::run(&env, session(), &call, &CancellationToken::new(), &limits).await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    assert!(
        output.text.starts_with("Already applied"),
        "{}",
        output.text
    );
    let entries = fs::list(&root, 10).await.expect("listing");
    let names: Vec<_> = entries.iter().map(|entry| entry.name.clone()).collect();
    assert_eq!(
        names,
        vec![".verified", "lib.rs"],
        "no staged file is left behind; the marker is in the daemon's directory"
    );

    let read = approved(ToolArgs::Read {
        path: ProjectPath::parse("missing.rs").expect("a path"),
    });
    let (exit, output) =
        tool::run(&env, session(), &read, &CancellationToken::new(), &limits).await;
    assert_eq!(exit, Exit::Err(Cause::Failed(Fault::Other)), "missing");
    assert!(output.text.contains("not found"), "{}", output.text);
}

#[tokio::test]
async fn file_log_survives_reopen_and_cuts_a_torn_tail() {
    let dir = scratch("log").await;
    let path = dir.join("session.log");
    let mut log = FileLog::create(&path, 1 << 20, 1 << 30)
        .await
        .expect("created");
    let group = |epoch: u64| Record::<Parsed>::EpochBumped {
        epoch: Epoch(epoch),
    };
    log.append(&[group(1), group(2)]).await.expect("durable");
    log.append(&[group(3)]).await.expect("durable");
    drop(log);

    // A torn write after the last acknowledged group.
    let mut file = fs::open_append(&path).await.expect("open");
    file.write_all(b"deadbeef 0000000000000003 00000")
        .await
        .expect("torn bytes");
    file.flush().await.expect("flushed");
    drop(file);

    let (mut log, records) = FileLog::open(&path, 1 << 20, 1 << 30)
        .await
        .expect("reopened");
    assert_eq!(
        records,
        vec![group(1), group(2), group(3)],
        "acknowledged records survive"
    );
    assert_eq!(log.next_seq(), 3, "the torn tail is cut");
    log.append(&[group(4)])
        .await
        .expect("appends after the cut");
    assert_eq!(
        log.read_from(2).await.expect("rereads"),
        vec![group(3), group(4)],
        "read_from"
    );
    let (_, records) = FileLog::open(&path, 1 << 20, 1 << 30).await.expect("clean");
    assert_eq!(records.len(), 4, "clean after the cut");
}

#[tokio::test]
async fn daemon_takes_the_lock_and_writes_the_port_file() {
    let project = scratch("daemon").await;
    let layout = Layout {
        project: project.clone(),
    };
    let world = World::new();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let seams = Seams {
        model: Arc::new(FakeModel::new(Arc::clone(&clock), 1000)),
        env: Arc::new(local(project.clone())),
        clock,
        entropy: Arc::new(OsEntropy),
        world: world.clone(),
    };
    let config = ShellConfig {
        limits: ServerLimits::v0(),
        model: "fake".to_owned(),
        system: String::new(),
        planted: verified_server::shell::Planted::default(),
    };
    let listener = net::bind("127.0.0.1:0".parse().expect("an addr"))
        .await
        .expect("bound");
    let run_layout = layout.clone();
    let (done, mut finished) = tokio::sync::mpsc::channel(1);
    world.spawn(async move {
        let result = daemon::run(listener, &run_layout, &FileLock, seams, config).await;
        drop(done.send(result).await);
    });
    let mut addr = None;
    for _ in 0..200u32 {
        addr = daemon::read_port_file(&layout).await.expect("readable");
        if addr.is_some() {
            break;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    let addr = addr.expect("the port file appears");
    assert_eq!(addr.ip().to_string(), "127.0.0.1", "localhost");
    let second = FileLock.lock(&layout.lock_file()).await;
    assert!(
        matches!(second, Err(LockError::Held { .. })),
        "one daemon per project: {second:?}"
    );
    world.shutdown().cancel();
    let result = within(Duration::from_secs(5), finished.recv())
        .await
        .expect("stops on shutdown")
        .expect("reported");
    assert!(
        matches!(result, Ok(()) | Err(DaemonError::Serve(_))),
        "{result:?}"
    );
    let again = FileLock.lock(&layout.lock_file()).await;
    assert!(again.is_ok(), "the lock is released when the daemon stops");
}

/// An edit that deletes (empty `new`) or whose `new` contains `old`: the
/// file's text cannot say whether a rerun already applied it, the marker
/// can. Neither a false "already applied" nor a double apply.
#[tokio::test]
async fn edit_reruns_are_decided_by_the_marker_not_the_text() {
    let root = scratch("edit-marker").await;
    fs::write_synced(&root.join("a.txt"), b"keep drop keep\n")
        .await
        .expect("a file");
    fs::write_synced(&root.join("b.txt"), b"x = 1\n")
        .await
        .expect("a file");
    let env = local(root.clone());
    let limits = ServerLimits::v0();
    let delete = approved(ToolArgs::Edit {
        path: ProjectPath::parse("a.txt").expect("a path"),
        old: "drop ".to_owned(),
        new: String::new(),
    });
    let none = CancellationToken::new();
    let (exit, output) = tool::run(&env, session(), &delete, &none, &limits).await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    let (exit, output) = tool::run(&env, session(), &delete, &none, &limits).await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    assert!(
        output.text.starts_with("Already applied"),
        "{}",
        output.text
    );
    let text = fs::read(&root.join("a.txt")).await.expect("read back");
    assert_eq!(text, b"keep keep\n", "deleted once");

    // A deletion whose text was never there fails, rather than claiming an
    // empty `new` is "already in place".
    let absent = approved(ToolArgs::Edit {
        path: ProjectPath::parse("b.txt").expect("a path"),
        old: "nowhere".to_owned(),
        new: String::new(),
    });
    let (exit, output) = tool::run(
        &env,
        SessionId::from_parts(at(), &[4; 10]),
        &absent,
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(
        exit,
        Exit::Err(Cause::Failed(Fault::Other)),
        "{}",
        output.text
    );

    // `new` contains `old`: a text check would apply it again.
    let grow = approved(ToolArgs::Edit {
        path: ProjectPath::parse("b.txt").expect("a path"),
        old: "1".to_owned(),
        new: "12".to_owned(),
    });
    let other = SessionId::from_parts(at(), &[5; 10]);
    for _ in 0..2u8 {
        let (exit, output) =
            tool::run(&env, other, &grow, &CancellationToken::new(), &limits).await;
        assert_eq!(exit, Exit::Ok, "{}", output.text);
    }
    let text = fs::read(&root.join("b.txt")).await.expect("read back");
    assert_eq!(text, b"x = 12\n", "applied exactly once");
}

/// A crash after the marker and before the rename: the rerun finishes the
/// rename with the staged text.
#[tokio::test]
async fn edit_rerun_finishes_a_marked_rename() {
    use verified_server::env::{Env as _, Grant};

    let root = scratch("edit-finish").await;
    fs::write_synced(&root.join("c.txt"), b"old\n")
        .await
        .expect("a file");
    let env = local(root.clone());
    let call = approved(ToolArgs::Edit {
        path: ProjectPath::parse("c.txt").expect("a path"),
        old: "old".to_owned(),
        new: "new".to_owned(),
    });
    let grant = match Grant::of(&call, session()) {
        Grant::Edit(grant) => Some(grant),
        Grant::Read(_) | Grant::Bash(_) => None,
    };
    let grant = grant.expect("an edit call grants an edit");
    env.stage(&grant, b"new\n").await.expect("staged");
    env.mark(&grant, Fingerprint::of(b"old\n"))
        .await
        .expect("marked");
    assert_eq!(
        env.marked(&grant).await.expect("readable"),
        Some(Fingerprint::of(b"old\n")),
        "marked with the original's fingerprint"
    );
    let (exit, output) = tool::run(
        &env,
        session(),
        &call,
        &CancellationToken::new(),
        &ServerLimits::v0(),
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    assert!(output.text.starts_with("Edited"), "{}", output.text);
    let text = fs::read(&root.join("c.txt")).await.expect("read back");
    assert_eq!(text, b"new\n", "the staged text landed");
}

/// The model of the crash test: its stream panics, in a world task.
#[derive(Debug)]
struct PanickingModel;

#[async_trait::async_trait]
impl verified_server::model::Model for PanickingModel {
    #[expect(
        clippy::panic,
        reason = "the test's point: a broken invariant inside a world task"
    )]
    async fn stream(
        &self,
        _request: &verified_server::model::ModelRequest,
        _parts: tokio::sync::mpsc::Sender<verified_server::payload::ModelPart>,
        _cancel: &CancellationToken,
    ) -> Result<(), verified_server::model::ModelError> {
        panic!("the model task panics on purpose");
    }
}

const PANIC_CHILD: &str = "VERIFIED_TEST_PANIC_CHILD";
const PANIC_TEST: &str = "a_panicking_task_aborts_the_daemon";

/// The child half: a daemon whose model task panics. Crash-only means the
/// process aborts; if it is still alive after the bound it exits cleanly,
/// which the parent reports as the failure.
fn panicking_daemon() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let world = World::new();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let limits = ServerLimits::v0();
        let seams = Seams {
            model: Arc::new(PanickingModel),
            env: Arc::new(LocalEnv::new(
                std::env::temp_dir(),
                Arc::clone(&clock),
                world.groups().clone(),
            )),
            clock,
            entropy: Arc::new(OsEntropy),
            world,
        };
        let config = ShellConfig {
            limits: limits.clone(),
            model: "fake".to_owned(),
            system: String::new(),
            planted: verified_server::shell::Planted::default(),
        };
        let store = Arc::new(verified_server::log::MemLogStore::new(
            verified_server::log::MemDisk::new(),
            limits.log_line_bytes,
            limits.log_file_bytes,
        ));
        let (fatal, _failed) = tokio::sync::mpsc::channel(1);
        let registry = daemon::boot(store, seams, Arc::new(config), fatal)
            .await
            .expect("booted");
        let (_, handle) = registry.create(session()).await.expect("a session");
        let message = verified_wire::command::SendMessage {
            message_id: MessageId::from_parts(at(), &[7; 10]),
            text: verified_wire::command::MessageText::parse(
                "hello".to_owned(),
                &verified_wire::WireLimits::DEFAULT,
            )
            .expect("text"),
        };
        handle.send_message(message).await.expect("accepted");
        SystemClock.sleep(Millis::new(10_000)).await;
    });
}

/// Crash-only in the shipped build: a panic in a world task (here the model
/// task) aborts the daemon process instead of leaving a half-dead daemon
/// serving. Runs the daemon in a child process (this test binary again).
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test re-runs its own binary as the daemon under test; the env var marks the child"
)]
fn a_panicking_task_aborts_the_daemon() {
    use std::os::unix::process::ExitStatusExt as _;

    if std::env::var_os(PANIC_CHILD).is_some() {
        panicking_daemon();
        return;
    }
    let exe = std::env::current_exe().expect("the test binary");
    let output = std::process::Command::new(exe)
        .args(["--exact", PANIC_TEST, "--nocapture", "--test-threads=1"])
        .env(PANIC_CHILD, "1")
        .output()
        .expect("the child ran");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the model task panics on purpose"),
        "the child reached the panic: {stderr}"
    );
    assert_eq!(
        output.status.signal(),
        Some(rustix::process::Signal::ABORT.as_raw()),
        "the daemon aborted rather than living on: {:?}\n{stderr}",
        output.status
    );
}

/// Whether process `pid` is gone (reaped): polled for up to five seconds.
async fn gone(pid: i32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return true;
    };
    for _ in 0..500u32 {
        if rustix::process::test_kill_process(pid).is_err() {
            return true;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    false
}

async fn pid_in(path: &std::path::Path) -> i32 {
    for _ in 0..500u32 {
        if let Ok(bytes) = fs::read(path).await
            && let Ok(pid) = String::from_utf8_lossy(&bytes).trim().parse()
        {
            return pid;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    unreachable_pid()
}

#[expect(
    clippy::panic,
    reason = "a test helper: the command never wrote its pid"
)]
fn unreachable_pid() -> i32 {
    panic!("the command did not write its pid")
}

/// A command that daemonizes a child holding its output (it leaves the
/// group with `setsid`) cannot hang the call: once the group is stopped the
/// output is drained for a bounded time, and the outcome is unknown.
#[tokio::test]
async fn a_daemonized_child_cannot_hang_bash() {
    let root = scratch("escape").await;
    let env = local(root.clone());
    let mut limits = ServerLimits::v0();
    limits.exec_timeout = Millis::new(300);
    limits.exec_drain = Millis::new(300);
    let call = bash(
        "perl -MPOSIX -e 'fork and exit; POSIX::setsid(); open(F, \">\", \"escaped.pid\"); print F $$; close F; sleep 20'; echo leader-done",
    );
    // The escaped child sleeps 20s holding the output: returning within 5s
    // is the bound at work.
    let (exit, output) = within(
        Duration::from_secs(5),
        tool::run(&env, session(), &call, &CancellationToken::new(), &limits),
    )
    .await
    .expect("bash returns while the escaped child still holds its output");
    let escaped = pid_in(&root.join("escaped.pid")).await;
    let alive = rustix::process::Pid::from_raw(escaped)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok());
    if let Some(pid) = rustix::process::Pid::from_raw(escaped) {
        let _cleanup: Result<(), rustix::io::Errno> =
            rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    assert!(alive, "the child escaped the group and outlived the call");
    assert_eq!(exit, Exit::Err(Cause::Unknown), "{}", output.text);
    assert!(output.text.contains("leader-done"), "{}", output.text);
    assert!(
        output.text.contains("may still be running"),
        "{}",
        output.text
    );

    // Cancelled rather than timed out: the same bound.
    let mut limits = ServerLimits::v0();
    limits.exec_drain = Millis::new(300);
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    let call = bash("perl -MPOSIX -e 'fork and exit; POSIX::setsid(); sleep 3'; sleep 30");
    let stop = async move {
        SystemClock.sleep(Millis::new(300)).await;
        stopper.cancel();
    };
    let ((exit, output), ()) = within(Duration::from_secs(10), async {
        tokio::join!(tool::run(&env, session(), &call, &cancel, &limits), stop)
    })
    .await
    .expect("cancel returns while the escaped child holds the output");
    assert_eq!(exit, Exit::Err(Cause::Unknown), "{}", output.text);
    assert!(output.text.contains("interrupted"), "{}", output.text);
}

/// The daemon stopping (the call's future dropped) or crashing (an abort,
/// after which nothing is dropped, so the world's registry is used) kills
/// a running command's whole process group, not only its leader.
#[tokio::test]
async fn a_stopping_daemon_kills_whole_process_groups() {
    let root = scratch("groups").await;
    let groups = ProcessGroups::default();
    let env = LocalEnv::new(root.clone(), Arc::new(SystemClock), groups.clone());
    let limits = ServerLimits::v0();

    // Dropped mid-run.
    let call = bash("sleep 30 & echo $! > dropped.pid; wait");
    let dropped = within(
        Duration::from_millis(500),
        tool::run(&env, session(), &call, &CancellationToken::new(), &limits),
    )
    .await;
    assert!(dropped.is_err(), "still running when dropped");
    let background = pid_in(&root.join("dropped.pid")).await;
    assert!(
        gone(background).await,
        "the background sleep died with the group"
    );
    assert!(groups.is_empty(), "nothing is left registered");

    // Killed through the registry, as the crash shim does before aborting.
    let call = bash("sleep 30 & echo $! > crashed.pid; wait");
    let crash = async {
        let background = pid_in(&root.join("crashed.pid")).await;
        assert_eq!(groups.len(), 1, "the running group is registered");
        groups.kill_all();
        background
    };
    let none = CancellationToken::new();
    let ((exit, output), background) = within(Duration::from_secs(10), async {
        tokio::join!(tool::run(&env, session(), &call, &none, &limits), crash)
    })
    .await
    .expect("the command ends once its group is killed");
    assert_eq!(
        exit,
        Exit::Err(Cause::Failed(Fault::Other)),
        "{}",
        output.text
    );
    assert!(gone(background).await, "the background sleep died too");
    assert!(groups.is_empty(), "a reaped group is unregistered");
}

/// Symlinks are never followed, on the path or at its end: an auto-approved
/// read cannot leave the project through one, and neither can an edit.
#[tokio::test]
async fn symlinks_never_lead_out_of_the_project() {
    let root = scratch("links").await;
    let outside = scratch("outside").await;
    fs::write_synced(&outside.join("secret.txt"), b"secret old\n")
        .await
        .expect("a file outside");
    std::os::unix::fs::symlink(&outside, root.join("link")).expect("a dir link");
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("file-link"))
        .expect("a file link");
    let env = local(root.clone());
    let limits = ServerLimits::v0();
    for path in ["link/secret.txt", "file-link", "link"] {
        let read = approved(ToolArgs::Read {
            path: ProjectPath::parse(path).expect("a path"),
        });
        let (exit, output) =
            tool::run(&env, session(), &read, &CancellationToken::new(), &limits).await;
        assert_eq!(exit, Exit::Err(Cause::Failed(Fault::Other)), "{path}");
        assert!(output.text.contains("symlink"), "{path}: {}", output.text);
        assert!(!output.text.contains("secret"), "{path}: {}", output.text);
    }
    let outside_edit = approved(ToolArgs::Edit {
        path: ProjectPath::parse("link/secret.txt").expect("a path"),
        old: "old".to_owned(),
        new: "new".to_owned(),
    });
    let (exit, output) = tool::run(
        &env,
        session(),
        &outside_edit,
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(
        exit,
        Exit::Err(Cause::Failed(Fault::Other)),
        "{}",
        output.text
    );
    let text = fs::read(&outside.join("secret.txt")).await.expect("read");
    assert_eq!(text, b"secret old\n", "untouched");
}

/// A rerun of a marked edit renames its staged copy only over the text it
/// was made from: if the file changed meanwhile, the edit fails and says so.
#[tokio::test]
async fn a_rerun_never_overwrites_a_file_changed_since_staging() {
    use verified_server::env::{Env as _, Grant};

    let root = scratch("edit-changed").await;
    fs::write_synced(&root.join("d.txt"), b"old\n")
        .await
        .expect("a file");
    let env = local(root.clone());
    let call = approved(ToolArgs::Edit {
        path: ProjectPath::parse("d.txt").expect("a path"),
        old: "old".to_owned(),
        new: "new".to_owned(),
    });
    let grant = match Grant::of(&call, session()) {
        Grant::Edit(grant) => Some(grant),
        Grant::Read(_) | Grant::Bash(_) => None,
    };
    let grant = grant.expect("an edit call grants an edit");
    env.stage(&grant, b"new\n").await.expect("staged");
    env.mark(&grant, Fingerprint::of(b"old\n"))
        .await
        .expect("marked");
    // Someone changes the file before the restarted daemon reruns the edit.
    fs::write_synced(&root.join("d.txt"), b"someone else's\n")
        .await
        .expect("changed");
    let (exit, output) = tool::run(
        &env,
        session(),
        &call,
        &CancellationToken::new(),
        &ServerLimits::v0(),
    )
    .await;
    assert_eq!(
        exit,
        Exit::Err(Cause::Failed(Fault::Other)),
        "{}",
        output.text
    );
    assert!(output.text.contains("changed"), "{}", output.text);
    let text = fs::read(&root.join("d.txt")).await.expect("read back");
    assert_eq!(text, b"someone else's\n", "not overwritten");
}

/// On a real disk: an append longer than a replayable line is refused and
/// writes nothing; what was acknowledged reopens exactly.
#[tokio::test]
async fn file_log_refuses_what_replay_would_cut() {
    let dir = scratch("log-bounds").await;
    let path = dir.join("session.log");
    let mut log = FileLog::create(&path, 512, 1 << 20).await.expect("created");
    let small = Record::<Parsed>::EpochBumped { epoch: Epoch(1) };
    let large = Record::<Parsed>::TurnStarted {
        turn: TurnId::from_parts(at(), &[1; 10]),
        message: MessageId::from_parts(at(), &[2; 10]),
        text: measure("x".repeat(600)),
    };
    log.append(std::slice::from_ref(&small))
        .await
        .expect("fits");
    let refused = log.append(&[large]).await;
    assert!(
        matches!(
            refused,
            Err(verified_server::log::LogError::LineTooLong { seq: 1, .. })
        ),
        "{refused:?}"
    );
    log.append(&[Record::EpochBumped { epoch: Epoch(2) }])
        .await
        .expect("the log is still usable");
    drop(log);
    let (_, records) = FileLog::open(&path, 512, 1 << 20).await.expect("reopens");
    assert_eq!(
        records,
        vec![small, Record::EpochBumped { epoch: Epoch(2) }],
        "exactly what was acknowledged"
    );
}
