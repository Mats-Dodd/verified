//! The real seams on the real OS: `LocalEnv` with `/bin/sh` and process
//! groups, `FileLog` on disk, and `daemon::run` with its lock and port file.
//! Without the `sim` feature only: with it, the fs shim is turmoil's.
#![cfg(not(feature = "sim"))]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::panic,
    clippy::zombie_processes,
    reason = "test harness code, not the code under test"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;
use verified_core::{Approved, Core, Effect, Event, ModelExit, ToolCall, Verdict};
use verified_domain::{
    ActorId, ActorName, Address, Body, Born, CallId, Envelope, EnvelopeId, Epoch, Exit, Limits,
    MessageId, Millis, Proposal, Record, Request, Tool, UnixMillis,
};
use verified_server::daemon::{self, DaemonError, Layout};
use verified_server::env::LocalEnv;
use verified_server::limits::ServerLimits;
use verified_server::log::{FileLog, Log as _};
use verified_server::payload::{Command, Parsed, ProjectPath, ToolArgs, Truncation, measure};
use verified_server::seam::{
    Clock, Entropy as _, FileLock, Lock as _, LockError, OsEntropy, ProcessGroups, SystemClock,
    Tasks,
};
use verified_server::shell::{Seams, ShellConfig};
use verified_server::shim::{fs, net};
use verified_server::tool;

use common::{fake_model, within};

async fn scratch(name: &str) -> PathBuf {
    let mut random = [0u8; 8];
    OsEntropy.fill(&mut random);
    let dir = std::env::temp_dir().join(format!(
        "verified-{name}-{}-{:x}",
        std::process::id(),
        u64::from_le_bytes(random)
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
    let actor = ActorId::from_parts(at(), &[3; 10]);
    let born = Born::root(
        actor,
        ActorName::parse("main").expect("a name"),
        vec![Tool::Read, Tool::Edit, Tool::Bash],
    );
    for record in &core.resume(Some(born), None).records {
        core.apply(record).expect("resumed records apply");
    }
    let started = core.decide(Event::Deliver {
        envelope: Envelope {
            id: EnvelopeId::User(MessageId::from_parts(at(), &[1; 10])),
            from: Address::User,
            to: actor,
            reply_to: None,
            body: Body::Text(measure("go".to_owned())),
        },
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

fn session() -> ActorId {
    ActorId::from_parts(at(), &[3; 10])
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
    assert_eq!(exit, Exit::Failed, "nonzero exit fails: {}", output.text);
    assert!(output.text.contains("id=toolu_test"), "{}", output.text);
    assert!(output.text.contains("oops"), "stderr is captured");
    assert!(output.text.ends_with("[exit status 3]"), "{}", output.text);
}

/// Cancelling a running command kills its whole process group, and the
/// model hears one honest message.
#[tokio::test]
async fn cancelling_bash_kills_its_group_and_reports_the_honest_message() {
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
    assert_eq!(exit, Exit::Unknown, "stopped mid-run: it may have acted");
    assert!(output.text.contains("started"), "{}", output.text);
    assert!(!output.text.contains("never"), "{}", output.text);
    assert!(
        output
            .text
            .ends_with("[stopped (cancelled); processes it started may still be running]"),
        "{}",
        output.text
    );
}

/// A command whose leader closes its output keeps running until it exits:
/// v sends no signal to a command nobody stopped, so its own status is
/// what the model hears.
#[tokio::test]
async fn a_leader_that_closes_its_output_still_runs_to_its_exit() {
    let root = scratch("closed").await;
    let env = local(root);
    let limits = ServerLimits::v0();
    let (exit, output) = tool::run(
        &env,
        session(),
        &bash("echo hi; exec >/dev/null 2>&1; sleep 0.3; exit 4"),
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(exit, Exit::Failed, "{}", output.text);
    assert!(output.text.contains("hi"), "{}", output.text);
    assert!(output.text.ends_with("[exit status 4]"), "{}", output.text);
}

/// A leader the stopping kill cannot reach (here: it moved itself out of
/// its group into the test's own, as an `EPERM` would leave it) does not
/// hang the call: the reap is bounded by the drain, like the output.
#[tokio::test]
async fn a_leader_the_kill_cannot_reach_does_not_hang_a_stopped_command() {
    let root = scratch("unreachable").await;
    let env = local(root);
    let mut limits = ServerLimits::v0();
    limits.exec_drain = Millis::new(200);
    // Holding its output, and having closed it.
    for closes in ["", "close STDOUT; close STDERR; "] {
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        let call = bash(&format!(
            "exec perl -e '$| = 1; setpgrp(0, getpgrp(getppid())) or die qq(setpgrp: $!); \
             print qq(moved\\n); {closes}sleep 5'"
        ));
        let stop = async move {
            SystemClock.sleep(Millis::new(500)).await;
            stopper.cancel();
        };
        let ((exit, output), ()) = within(Duration::from_secs(3), async {
            tokio::join!(tool::run(&env, session(), &call, &cancel, &limits), stop)
        })
        .await
        .expect("the reap is bounded although the leader outlives the kill");
        assert_eq!(exit, Exit::Unknown, "{}", output.text);
        assert!(output.text.contains("moved"), "{}", output.text);
        assert!(
            output.text.contains("may still be running"),
            "{}",
            output.text
        );
    }
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
    assert_eq!(exit, Exit::Unknown, "timed out: it may have acted");
    assert!(
        output
            .text
            .ends_with("[stopped (timed out); processes it started may still be running]"),
        "{}",
        output.text
    );

    // The default timeout: under load, 300 ms can cut even a short command.
    let limits = ServerLimits {
        tool_output_bytes: 1000,
        ..ServerLimits::v0()
    };
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

/// An edit replaces the file's contents, not its mode: an executable
/// script stays executable.
#[tokio::test]
async fn an_edit_keeps_the_mode() {
    let root = scratch("mode").await;
    let file = root.join("run.sh");
    fs::write_synced(&file, b"echo hi\n").await.expect("a file");
    rustix::fs::chmod(&file, rustix::fs::Mode::from_raw_mode(0o755)).expect("chmod");
    let env = local(root.clone());
    let call = approved(ToolArgs::Edit {
        path: ProjectPath::parse("run.sh").expect("a path"),
        old: "hi".to_owned(),
        new: "yo".to_owned(),
    });
    let (exit, output) = tool::run(
        &env,
        session(),
        &call,
        &CancellationToken::new(),
        &ServerLimits::v0(),
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    let mode = rustix::fs::stat(&file).expect("stat").st_mode & 0o777;
    assert_eq!(mode, 0o755, "the mode is now {mode:o}");
}

#[tokio::test]
async fn edit_is_compare_and_swap() {
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
    let entries = fs::list(&root, 10).await.expect("listing");
    let names: Vec<_> = entries.iter().map(|entry| entry.name.clone()).collect();
    assert_eq!(names, vec!["lib.rs"], "no staged file is left behind");
    // The same edit again: `old` is gone, so it fails rather than applying
    // twice.
    let (exit, output) =
        tool::run(&env, session(), &call, &CancellationToken::new(), &limits).await;
    assert_eq!(exit, Exit::Failed, "{}", output.text);
    assert!(output.text.contains("not found"), "{}", output.text);
    let text = fs::read(&root.join("lib.rs")).await.expect("read back");
    assert_eq!(text, b"fn main() {}\n", "unchanged");

    let read = approved(ToolArgs::Read {
        path: ProjectPath::parse("missing.rs").expect("a path"),
    });
    let (exit, output) =
        tool::run(&env, session(), &read, &CancellationToken::new(), &limits).await;
    assert_eq!(exit, Exit::Failed, "missing");
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
    log.append(&[group(1), group(2)], 0).await.expect("durable");
    log.append(&[group(3)], 0).await.expect("durable");
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
    log.append(&[group(4)], 0)
        .await
        .expect("appends after the cut");
    let (_, records) = FileLog::open(&path, 1 << 20, 1 << 30).await.expect("clean");
    assert_eq!(
        records,
        vec![group(1), group(2), group(3), group(4)],
        "clean after the cut, holding what was appended after it"
    );
}

#[tokio::test]
async fn daemon_takes_the_lock_and_writes_the_port_file() {
    let project = scratch("daemon").await;
    let layout = Layout {
        project: project.clone(),
    };
    let tasks = Tasks::new();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let seams = Seams {
        model: Arc::new(fake_model(&ServerLimits::v0(), &clock, &tasks).await),
        env: Arc::new(local(project.clone())),
        clock,
        entropy: Arc::new(OsEntropy),
        tasks: tasks.clone(),
    };
    let config = ShellConfig {
        limits: ServerLimits::v0(),
        model: "fake".to_owned(),
        system: String::new(),
        planted: verified_server::shell::Planted::None,
    };
    let listener = net::bind("127.0.0.1:0".parse().expect("an addr"))
        .await
        .expect("bound");
    let run_layout = layout.clone();
    let (done, mut finished) = tokio::sync::mpsc::channel(1);
    tasks.spawn(async move {
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
    tasks.shutdown().cancel();
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

/// The model of the crash test: its stream panics, in a daemon task.
#[derive(Debug)]
struct PanickingModel;

#[async_trait::async_trait]
impl verified_server::model::Model for PanickingModel {
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
        let tasks = Tasks::new();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let limits = ServerLimits::v0();
        let seams = Seams {
            model: Arc::new(PanickingModel),
            env: Arc::new(LocalEnv::new(
                std::env::temp_dir(),
                Arc::clone(&clock),
                tasks.groups().clone(),
            )),
            clock,
            entropy: Arc::new(OsEntropy),
            tasks,
        };
        let config = ShellConfig {
            limits: limits.clone(),
            model: "fake".to_owned(),
            system: String::new(),
            planted: verified_server::shell::Planted::None,
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

/// This test binary again, running only `test` as the child half of a
/// crash test.
fn run_child(test: &str) -> std::process::Output {
    let exe = std::env::current_exe().expect("the test binary");
    std::process::Command::new(exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(PANIC_CHILD, "1")
        .output()
        .expect("the child ran")
}

fn is_child() -> bool {
    std::env::var_os(PANIC_CHILD).is_some()
}

/// Crash-only in the shipped build: a panic in a daemon task (here the model
/// task) aborts the daemon process instead of leaving a half-dead daemon
/// serving. Runs the daemon in a child process (this test binary again).
#[test]
fn a_panicking_task_aborts_the_daemon() {
    use std::os::unix::process::ExitStatusExt as _;

    if is_child() {
        panicking_daemon();
        return;
    }
    let output = run_child(PANIC_TEST);
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

const GROUPS_TEST: &str = "a_panic_anywhere_kills_the_running_commands";

/// The child half: a running command's process group is registered with
/// the daemon's tasks, as `LocalEnv` does, the daemon's panic hook is installed, and
/// a plain thread (no daemon task, so no `catch_unwind`) panics. The hook
/// aborts the process: nothing is dropped.
fn panicking_thread() {
    use std::os::unix::process::CommandExt as _;

    let tasks = Tasks::new();
    let command = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 300 & wait"])
        .process_group(0)
        // Not the test's pipes: the parent reads them to their end.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("a command");
    let group = i32::try_from(command.id()).expect("a pid");
    std::io::Write::write_all(
        &mut std::io::stdout(),
        format!("group {group}\n").as_bytes(),
    )
    .expect("the parent reads it");
    tasks.groups().insert(group);
    tasks.groups().kill_on_panic();
    let panicked = std::thread::spawn(|| {
        panic!("a plain thread panics on purpose");
    })
    .join();
    drop(panicked);
}

/// A panic anywhere in the daemon, not only in a daemon task, kills every
/// running command's process group before the process dies: under
/// `panic = "abort"` nothing is dropped and no `catch_unwind` runs, so the
/// panic hook is all that is left.
#[tokio::test]
async fn a_panic_anywhere_kills_the_running_commands() {
    if is_child() {
        panicking_thread();
        return;
    }
    let output = run_child(GROUPS_TEST);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a plain thread panics on purpose"),
        "the child reached the panic: {stderr}"
    );
    // The harness prints the test's name on the same line.
    let group = stdout
        .split_once("group ")
        .and_then(|(_, rest)| rest.lines().next()?.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("the child's command group");
    let mut alive = true;
    for _ in 0..500u32 {
        alive = rustix::process::test_kill_process_group(group).is_ok();
        if !alive {
            break;
        }
        SystemClock.sleep(Millis::new(10)).await;
    }
    if alive {
        let _cleanup: Result<(), rustix::io::Errno> =
            rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    assert!(
        !alive,
        "the command's group outlived the panic: {stdout}\n{stderr}"
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

fn unreachable_pid() -> i32 {
    panic!("the command did not write its pid")
}

/// A command that daemonizes a child holding its output (it leaves the
/// group with `setsid`) cannot hang the call: once the group is stopped the
/// output is drained for a bounded time, and the message says the child may
/// still run.
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
    assert_eq!(exit, Exit::Unknown, "{}", output.text);
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
    assert_eq!(exit, Exit::Unknown, "{}", output.text);
    assert!(
        output.text.contains("may still be running"),
        "{}",
        output.text
    );
}

/// The daemon stopping (the call's future dropped) or crashing (an abort,
/// after which nothing is dropped, so the daemon's registry is used) kills
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

    // Killed through the registry, as the panic hook does before an abort.
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
    assert_eq!(exit, Exit::Failed, "{}", output.text);
    assert!(gone(background).await, "the background sleep died too");
    assert!(groups.is_empty(), "a reaped group is unregistered");
}

/// Symlinks are followed, on the path and at its end: `LocalEnv` contains
/// nothing. An edit through a link replaces the file it names, and the link
/// stays a link.
#[tokio::test]
async fn symlinks_are_followed() {
    let root = scratch("links").await;
    let outside = scratch("outside").await;
    fs::write_synced(&outside.join("shared.txt"), b"shared old\n")
        .await
        .expect("a file outside");
    std::os::unix::fs::symlink(&outside, root.join("link")).expect("a dir link");
    std::os::unix::fs::symlink(outside.join("shared.txt"), root.join("file-link"))
        .expect("a file link");
    let env = local(root.clone());
    let limits = ServerLimits::v0();
    for path in ["link/shared.txt", "file-link"] {
        let read = approved(ToolArgs::Read {
            path: ProjectPath::parse(path).expect("a path"),
        });
        let (exit, output) =
            tool::run(&env, session(), &read, &CancellationToken::new(), &limits).await;
        assert_eq!(exit, Exit::Ok, "{path}: {}", output.text);
        assert_eq!(output.text, "shared old\n", "{path}");
    }
    let through_link = approved(ToolArgs::Edit {
        path: ProjectPath::parse("file-link").expect("a path"),
        old: "old".to_owned(),
        new: "new".to_owned(),
    });
    let (exit, output) = tool::run(
        &env,
        session(),
        &through_link,
        &CancellationToken::new(),
        &limits,
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{}", output.text);
    let text = fs::read(&outside.join("shared.txt")).await.expect("read");
    assert_eq!(text, b"shared new\n", "the linked file changed");
    let links = fs::list(&root, 10).await.expect("listing");
    assert!(
        links
            .iter()
            .any(|entry| entry.name == "file-link" && entry.kind == fs::Kind::Symlink),
        "the link stays a link: {links:?}"
    );
    let entries = fs::list(&outside, 10).await.expect("listing");
    assert_eq!(entries.len(), 1, "no staged file is left beside it");
}

/// On a real disk: an append longer than a replayable line is refused and
/// writes nothing; what was acknowledged reopens exactly.
#[tokio::test]
async fn file_log_refuses_what_replay_would_cut() {
    let dir = scratch("log-bounds").await;
    let path = dir.join("session.log");
    let mut log = FileLog::create(&path, 512, 1 << 20).await.expect("created");
    let small = Record::<Parsed>::EpochBumped { epoch: Epoch(1) };
    let large = Record::<Parsed>::Delivered {
        envelope: Envelope {
            id: EnvelopeId::User(MessageId::from_parts(at(), &[2; 10])),
            from: Address::User,
            to: ActorId::from_parts(at(), &[3; 10]),
            reply_to: None,
            body: Body::Text(measure("x".repeat(600))),
        },
    };
    log.append(std::slice::from_ref(&small), 0)
        .await
        .expect("fits");
    let refused = log.append(&[large], 0).await;
    assert!(
        matches!(
            refused,
            Err(verified_server::log::LogError::LineTooLong { seq: 1, .. })
        ),
        "{refused:?}"
    );
    log.append(&[Record::EpochBumped { epoch: Epoch(2) }], 0)
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
