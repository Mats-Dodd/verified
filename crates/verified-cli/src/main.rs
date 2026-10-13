//! The `verified` command-line interface.
//!
//! `verified` runs the TUI, auto-starting the project's daemon when no live
//! one answers, and `verified daemon` runs the daemon. Both live in this one
//! binary so the client and daemon never skew versions; the TUI still checks
//! the daemon's `/version` and refuses a daemon from another build.
//!
//! This is the boundary: configuration and `ANTHROPIC_API_KEY` are read here,
//! once, and passed in as data.

use std::io::{self, IsTerminal as _};
use std::net::SocketAddr;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use verified_domain::Millis;
use verified_domain::id::SessionId;
use verified_server::daemon::{self, DaemonError, Layout};
use verified_server::env::LocalEnv;
use verified_server::limits::ServerLimits;
use verified_server::model::anthropic::{AnthropicConfig, AnthropicModel, ApiKey, BuildError};
use verified_server::seam::{Clock as _, FileLock, OsEntropy, SystemClock, World};
use verified_server::shell::{Seams, ShellConfig};
use verified_server::shim::net;
use verified_tui::app::Start;
use verified_tui::seam::TerminalError;
use verified_tui::terminal::{CrosstermInput, CrosstermScreen};

// The shipped binary never links the simulation shims. Cargo unifies features
// across one build, so building verified-sim next to this binary (`cargo
// build --workspace`, `--all-features`) turns on the `sim` features of
// verified-server and verified-tui here too; that build must fail, not ship
// a daemon on turmoil. Build the binary alone (`cargo build --release`, whose
// default members leave out verified-sim, or `-p verified-cli`).
const _: () = assert!(
    !verified_server::shim::SIMULATED && !verified_tui::shim::SIMULATED,
    "verified-cli was built with the `sim` shims on: build it without verified-sim \
     (`cargo build --release`, or `-p verified-cli`), never with `--workspace` or `--all-features`"
);

/// The model `verified daemon` asks unless told otherwise.
const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Where Anthropic's API lives.
const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
/// The system prompt of every session.
const SYSTEM: &str = "You are verified, a coding agent working in the user's project. \
    Use the read, edit and bash tools to inspect and change it.";
/// How many times the TUI looks for a daemon it started, and how long apart.
const DAEMON_START_ATTEMPTS: u32 = 50;
const DAEMON_START_PAUSE: Millis = Millis::new(100);
/// How long a `/version` check may take.
const VERSION_TIMEOUT: Millis = Millis::new(2_000);
/// The largest `/version` answer read.
const VERSION_BYTES: u64 = 4_096;

#[derive(Debug, Parser)]
#[command(
    name = "verified",
    version,
    about = "A coding agent built to be checked"
)]
struct Args {
    /// The project directory (default: the current directory).
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
    /// Opens this session instead of starting a new one.
    #[arg(long)]
    session: Option<SessionId>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Runs the project's daemon in the foreground.
    Daemon {
        /// The Anthropic model to use.
        #[arg(long, default_value = DEFAULT_MODEL)]
        model: String,
    },
}

/// Why `verified` stopped.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("starting the async runtime: {0}")]
    Runtime(#[source] io::Error),
    #[error("finding the project directory: {0}")]
    Project(#[source] io::Error),
    #[error("ANTHROPIC_API_KEY is not set")]
    MissingKey,
    #[error(transparent)]
    Model(#[from] BuildError),
    #[error("binding the daemon's port: {0}")]
    Bind(#[source] io::Error),
    #[error(transparent)]
    Daemon(#[from] DaemonError),
    #[error("starting the daemon: {0}")]
    Spawn(#[source] io::Error),
    #[error("the daemon did not come up")]
    NoDaemon,
    #[error("the running daemon is version {daemon}, this is {ours}; stop it and try again")]
    Skew { daemon: String, ours: &'static str },
    #[error("the TUI needs a terminal; run `verified daemon` for the daemon alone")]
    NotATerminal,
    #[error(transparent)]
    Tui(#[from] TerminalError),
}

fn runtime() -> Result<tokio::runtime::Runtime, CliError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(CliError::Runtime)
}

#[expect(
    clippy::disallowed_methods,
    reason = "main is the boundary where the API key enters, read once and passed on as data"
)]
fn api_key() -> Result<ApiKey, CliError> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .map(ApiKey::new)
        .ok_or(CliError::MissingKey)
}

async fn run_daemon(project: PathBuf, model: String, api_key: ApiKey) -> Result<(), CliError> {
    let limits = ServerLimits::v0();
    let base = ANTHROPIC_BASE
        .parse::<hyper::Uri>()
        .map_err(|error| BuildError::Endpoint(error.to_string()))?;
    let clock: Arc<dyn verified_server::seam::Clock> = Arc::new(SystemClock);
    let anthropic = AnthropicModel::https(
        AnthropicConfig {
            base,
            api_key,
            model: model.clone(),
        },
        &limits,
        Arc::clone(&clock),
    )?;
    // The world's process-group registry goes to the Env, so a daemon that
    // panics kills the commands it started.
    let world = World::new();
    world.groups().kill_on_panic();
    let seams = Seams {
        model: Arc::new(anthropic),
        env: Arc::new(LocalEnv::new(
            project.clone(),
            Arc::clone(&clock),
            world.groups().clone(),
        )),
        clock,
        entropy: Arc::new(OsEntropy),
        world,
    };
    let listener = net::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .map_err(CliError::Bind)?;
    let config = ShellConfig {
        limits,
        model,
        system: SYSTEM.to_owned(),
        planted: verified_server::shell::Planted::default(),
    };
    daemon::run(listener, &Layout { project }, &FileLock, seams, config).await?;
    Ok(())
}

/// The version a daemon at `addr` reports, if it answers.
#[expect(
    clippy::disallowed_types,
    reason = "the binary's port probe, before any seam exists; the shipped binary never runs under sim"
)]
async fn daemon_version(addr: SocketAddr) -> Option<String> {
    let ask = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
        stream
            .write_all(b"GET /version HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut answer = Vec::new();
        stream
            .take(VERSION_BYTES)
            .read_to_end(&mut answer)
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&answer);
        let (head, body) = text.split_once("\r\n\r\n")?;
        if !head.starts_with("HTTP/1.1 200") {
            return None;
        }
        let json: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
        json.get("version")?.as_str().map(str::to_owned)
    };
    tokio::select! {
        biased;
        version = ask => version,
        () = SystemClock.sleep(VERSION_TIMEOUT) => None,
    }
}

/// A live daemon of this build at the port file's address, if there is one.
async fn live_daemon(layout: &Layout) -> Result<Option<SocketAddr>, CliError> {
    let Some(addr) = daemon::read_port_file(layout).await? else {
        return Ok(None);
    };
    match daemon_version(addr).await {
        None => Ok(None),
        Some(version) if version == env!("CARGO_PKG_VERSION") => Ok(Some(addr)),
        Some(daemon) => Err(CliError::Skew {
            daemon,
            ours: env!("CARGO_PKG_VERSION"),
        }),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the CLI boundary starts the daemon process; tools' processes start only in the Env seam"
)]
fn start_daemon(project: &std::path::Path) -> Result<(), CliError> {
    let exe = std::env::current_exe().map_err(CliError::Spawn)?;
    let child = std::process::Command::new(exe)
        .arg("daemon")
        .arg("--project")
        .arg(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: closing the terminal hangs up the TUI's
        // group, not the daemon.
        .process_group(0)
        .spawn()
        .map_err(CliError::Spawn)?;
    // The daemon outlives this process; it is reaped by init when we exit.
    drop(child);
    Ok(())
}

/// The address of the project's daemon, starting one if none answers.
async fn ensure_daemon(layout: &Layout) -> Result<SocketAddr, CliError> {
    if let Some(addr) = live_daemon(layout).await? {
        return Ok(addr);
    }
    start_daemon(&layout.project)?;
    for _ in 0..DAEMON_START_ATTEMPTS {
        SystemClock.sleep(DAEMON_START_PAUSE).await;
        if let Some(addr) = live_daemon(layout).await? {
            return Ok(addr);
        }
    }
    Err(CliError::NoDaemon)
}

async fn run_tui(project: PathBuf, session: Option<SessionId>) -> Result<(), CliError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CliError::NotATerminal);
    }
    let layout = Layout { project };
    let addr = ensure_daemon(&layout).await?;
    let clock = verified_tui::seam::SystemClock;
    let mut entropy = verified_tui::seam::OsEntropy;
    let start = session.map_or_else(
        || Start::New(verified_tui::new_session(&clock, &mut entropy)),
        Start::Open,
    );
    let seams = verified_tui::tea::Seams {
        input: Box::new(CrosstermInput::new()),
        dialer: Arc::new(verified_tui::seam::TcpDialer::new(
            addr.ip().to_string(),
            addr.port(),
        )),
        clock: Arc::new(clock),
        entropy: Box::new(entropy),
    };
    let mut screen = CrosstermScreen::enter()?;
    let ran = verified_tui::run(
        start,
        verified_tui::Limits::DEFAULT,
        seams,
        &mut screen,
        &mut |_| {},
    )
    .await;
    screen.leave()?;
    ran?;
    Ok(())
}

fn main() -> Result<(), CliError> {
    let args = Args::parse();
    let project = match args.project {
        Some(project) => project,
        None => std::env::current_dir().map_err(CliError::Project)?,
    };
    match args.command {
        Some(Command::Daemon { model }) => {
            let key = api_key()?;
            runtime()?.block_on(run_daemon(project, model, key))
        }
        None => runtime()?.block_on(run_tui(project, args.session)),
    }
}
