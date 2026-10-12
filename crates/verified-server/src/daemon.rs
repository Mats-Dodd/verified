//! Daemon lifecycle: the `.verified/` directory, lock file and port file,
//! the session registry, and [`run`], the entry point main and the
//! simulation call with their seams.
//!
//! Layout, inside the project:
//!
//! ```text
//! .verified/
//!   daemon.lock          held (Lock seam) while a daemon runs
//!   port                 `127.0.0.1:PORT\n`, written atomically
//!   sessions/<id>.log    one record log per session
//! ```

use core::fmt;
use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use verified_domain::id::SessionId;
use verified_wire::command::Outcome as WireOutcome;

use crate::limits::ServerLimits;
use crate::log::{FileLogStore, LogError, LogStore};
use crate::routes;
use crate::seam::{Lock, LockError, LockGuard};
use crate::shell::{Seams, Session, SessionHandle, ShellConfig, ShellError};
use crate::shim::fs;

/// The daemon's directory inside a project.
pub const DIR: &str = ".verified";
/// The lock file, under [`DIR`].
pub const LOCK_FILE: &str = "daemon.lock";
/// The port file, under [`DIR`].
pub const PORT_FILE: &str = "port";
/// The session logs' directory, under [`DIR`].
pub const SESSIONS_DIR: &str = "sessions";

/// Why the daemon stopped.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("{what} {path}: {source}")]
    Io {
        what: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Limits(#[from] crate::limits::LimitsError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("session {session}: {source}")]
    Shell {
        session: SessionId,
        #[source]
        source: ShellError,
    },
    #[error("serving HTTP: {0}")]
    Serve(#[source] io::Error),
    /// A world task panicked (under the `sim` feature; the shipped daemon
    /// aborts instead). Crash-only: recovery on restart is the way back.
    #[error("a daemon task panicked")]
    Panicked,
}

fn io_error(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> DaemonError + use<> {
    let path = path.display().to_string();
    move |source| DaemonError::Io { what, path, source }
}

/// Why a session was not created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("the daemon hosts its limit of {limit} sessions")]
    Full { limit: usize },
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("the daemon is stopping")]
    Unavailable,
}

enum RegistryInput {
    Create {
        session: SessionId,
        reply: oneshot::Sender<Result<(WireOutcome, SessionHandle), CreateError>>,
    },
    Lookup {
        session: SessionId,
        reply: oneshot::Sender<Option<SessionHandle>>,
    },
}

/// The sessions a daemon hosts. A handle to the registry task.
#[derive(Clone)]
pub struct Registry {
    inbox: mpsc::Sender<RegistryInput>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Registry").finish_non_exhaustive()
    }
}

impl Registry {
    /// Creates `session` (durably) or finds it: `Exists` on a repeat.
    ///
    /// # Errors
    /// Too many sessions, a log that could not be created, or shutdown.
    pub async fn create(
        &self,
        session: SessionId,
    ) -> Result<(WireOutcome, SessionHandle), CreateError> {
        let (reply, answer) = oneshot::channel();
        self.inbox
            .send(RegistryInput::Create { session, reply })
            .await
            .map_err(|_closed| CreateError::Unavailable)?;
        answer.await.map_err(|_dropped| CreateError::Unavailable)?
    }

    /// The session, if the daemon hosts it.
    pub async fn lookup(&self, session: SessionId) -> Option<SessionHandle> {
        let (reply, answer) = oneshot::channel();
        self.inbox
            .send(RegistryInput::Lookup { session, reply })
            .await
            .ok()?;
        answer.await.ok().flatten()
    }
}

struct RegistryTask {
    sessions: BTreeMap<SessionId, SessionHandle>,
    store: Arc<dyn LogStore>,
    seams: Seams,
    config: Arc<ShellConfig>,
    fatal: mpsc::Sender<DaemonError>,
}

impl RegistryTask {
    fn spawn_session(
        &mut self,
        session: SessionId,
        log: Box<dyn crate::log::Log>,
        records: Vec<verified_domain::Record<crate::payload::Parsed>>,
    ) -> Result<SessionHandle, DaemonError> {
        let (handle, running) = Session::start(
            session,
            log,
            records,
            self.seams.clone(),
            Arc::clone(&self.config),
        )
        .map_err(|source| DaemonError::Shell { session, source })?;
        let fatal = self.fatal.clone();
        self.seams.world.spawn(async move {
            if let Err(source) = running.await {
                drop(fatal.try_send(DaemonError::Shell { session, source }));
            }
        });
        self.sessions.insert(session, handle.clone());
        Ok(handle)
    }

    async fn create(
        &mut self,
        session: SessionId,
    ) -> Result<(WireOutcome, SessionHandle), CreateError> {
        if let Some(handle) = self.sessions.get(&session) {
            return Ok((WireOutcome::Exists, handle.clone()));
        }
        let limit = self.config.limits.sessions;
        if self.sessions.len() >= limit {
            return Err(CreateError::Full { limit });
        }
        let log = self.store.create(session).await?;
        let handle = self
            .spawn_session(session, log, Vec::new())
            .unwrap_or_else(|error| {
                crate::defect(&format!("an empty log always replays: {error}"))
            });
        Ok((WireOutcome::Created, handle))
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<RegistryInput>) {
        let shutdown = self.seams.world.shutdown().clone();
        loop {
            let input = tokio::select! {
                biased;
                input = inbox.recv() => input,
                () = shutdown.cancelled() => return,
            };
            match input {
                None => return,
                Some(RegistryInput::Create { session, reply }) => {
                    let created = self.create(session).await;
                    drop(reply.send(created));
                }
                Some(RegistryInput::Lookup { session, reply }) => {
                    drop(reply.send(self.sessions.get(&session).cloned()));
                }
            }
        }
    }
}

/// Recovers every session in `store` and starts the registry.
///
/// # Errors
/// Limits under which some record would not fit the log, or a log that
/// does not open or replay.
pub async fn boot(
    store: Arc<dyn LogStore>,
    seams: Seams,
    config: Arc<ShellConfig>,
    fatal: mpsc::Sender<DaemonError>,
) -> Result<Registry, DaemonError> {
    config.limits.check()?;
    let (inbox, receiver) = mpsc::channel(config.limits.registry_inbox);
    let mut task = RegistryTask {
        sessions: BTreeMap::new(),
        store: Arc::clone(&store),
        seams: seams.clone(),
        config,
        fatal,
    };
    let sessions = store.list().await?;
    if sessions.len() > task.config.limits.sessions {
        return Err(DaemonError::Log(LogError::TooLarge {
            path: "sessions".to_owned(),
            length: u64::try_from(sessions.len()).unwrap_or(u64::MAX),
            limit: u64::try_from(task.config.limits.sessions).unwrap_or(u64::MAX),
        }));
    }
    for session in sessions {
        let (log, records) = store.open(session).await?;
        task.spawn_session(session, log, records)?;
    }
    seams.world.spawn(task.run(receiver));
    Ok(Registry { inbox })
}

/// Serves the daemon on `listener` with sessions from `store` until the
/// world shuts down or a session fails.
///
/// # Errors
/// Recovery failed, a session failed (crash-only), or serving failed.
pub async fn serve<L>(
    listener: L,
    store: Arc<dyn LogStore>,
    seams: Seams,
    config: ShellConfig,
) -> Result<(), DaemonError>
where
    L: axum::serve::Listener,
    L::Addr: fmt::Debug,
{
    let config = Arc::new(config);
    let (fatal, mut failed) = mpsc::channel(1);
    let registry = boot(store, seams.clone(), Arc::clone(&config), fatal).await?;
    let app = routes::router(routes::App::new(registry, seams.clone(), &config.limits));
    let stop = seams.world.shutdown().clone();
    let serving = axum::serve(listener, app).with_graceful_shutdown(async move {
        stop.cancelled().await;
    });
    // A failed or panicked task is fatal: the daemon stops serving (dropping
    // the server is the crash) and recovery on restart is the way back. The
    // crash is checked first: it also fires `shutdown`, which ends `serving`
    // gracefully, and must not read as a clean stop.
    let crashed = seams.world.crashed().clone();
    tokio::select! {
        biased;
        () = crashed.cancelled() => Err(DaemonError::Panicked),
        error = failed.recv() => {
            seams.world.shutdown().cancel();
            error.map_or(Ok(()), Err)
        }
        served = serving => match served {
            Ok(()) if crashed.is_cancelled() => Err(DaemonError::Panicked),
            Ok(()) => Ok(()),
            Err(error) => Err(DaemonError::Serve(error)),
        },
    }
}

/// Where a project's daemon keeps its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub project: PathBuf,
}

impl Layout {
    #[must_use]
    pub fn dir(&self) -> PathBuf {
        self.project.join(DIR)
    }

    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.dir().join(LOCK_FILE)
    }

    #[must_use]
    pub fn port_file(&self) -> PathBuf {
        self.dir().join(PORT_FILE)
    }

    #[must_use]
    pub fn sessions(&self) -> PathBuf {
        self.dir().join(SESSIONS_DIR)
    }
}

/// Creates `dir` (and its parents) and makes its entry durable.
async fn durable_dir(dir: &Path) -> Result<(), DaemonError> {
    fs::create_dir_all(dir)
        .await
        .map_err(io_error("creating", dir))?;
    if let Some(parent) = dir.parent() {
        fs::sync_dir(parent)
            .await
            .map_err(io_error("syncing", parent))?;
    }
    Ok(())
}

/// Writes the port file atomically: temp file, sync, rename, sync the dir.
///
/// # Errors
/// Any IO error.
pub async fn write_port_file(layout: &Layout, addr: SocketAddr) -> Result<(), DaemonError> {
    let dir = layout.dir();
    let staged = dir.join(format!("{PORT_FILE}.tmp"));
    let port = layout.port_file();
    fs::write_synced(&staged, format!("{addr}\n").as_bytes())
        .await
        .map_err(io_error("writing", &staged))?;
    fs::rename(&staged, &port)
        .await
        .map_err(io_error("renaming", &port))?;
    fs::sync_dir(&dir).await.map_err(io_error("syncing", &dir))
}

/// The address in a port file's text.
#[must_use]
pub fn parse_port_file(text: &str) -> Option<SocketAddr> {
    text.trim().parse().ok()
}

/// Reads the daemon's address from the port file, if there is one.
///
/// # Errors
/// IO errors other than not-found.
pub async fn read_port_file(layout: &Layout) -> Result<Option<SocketAddr>, DaemonError> {
    let path = layout.port_file();
    match fs::read(&path).await {
        Ok(bytes) => Ok(parse_port_file(&String::from_utf8_lossy(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("reading", &path)(error)),
    }
}

/// Prepares `layout.project` for a daemon listening on `addr`.
///
/// Makes `.verified/` durable, takes the lock, makes the sessions directory
/// durable and writes the port file. Returns the held lock (dropping it
/// releases the project) and the session logs' store.
///
/// # Errors
/// Another daemon holds the lock, or IO errors.
pub async fn prepare(
    addr: SocketAddr,
    layout: &Layout,
    lock: &dyn Lock,
    limits: &ServerLimits,
) -> Result<(LockGuard, FileLogStore), DaemonError> {
    durable_dir(&layout.dir()).await?;
    let held = lock.lock(&layout.lock_file()).await?;
    durable_dir(&layout.sessions()).await?;
    write_port_file(layout, addr).await?;
    let store = FileLogStore::new(
        layout.sessions(),
        limits.log_line_bytes,
        limits.log_file_bytes,
        limits.sessions,
    );
    Ok((held, store))
}

/// Runs the daemon for `layout.project`.
///
/// [`prepare`]s the project, recovers every session and serves. Main passes
/// the real seams and a bound localhost listener; the simulation passes
/// turmoil's.
///
/// # Errors
/// Another daemon holds the lock, IO errors, recovery failed, a session
/// failed, or serving failed.
pub async fn run<L>(
    listener: L,
    layout: &Layout,
    lock: &dyn Lock,
    seams: Seams,
    config: ShellConfig,
) -> Result<(), DaemonError>
where
    L: axum::serve::Listener<Addr = SocketAddr>,
{
    let addr = listener
        .local_addr()
        .map_err(io_error("reading the address of", &layout.project))?;
    let (_held, store) = prepare(addr, layout, lock, &config.limits).await?;
    serve(listener, Arc::new(store), seams, config).await
}
