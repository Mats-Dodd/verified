//! Daemon lifecycle: the `.verified/` directory, lock file and port file,
//! the actor registry, and [`run`], the entry point main and the
//! simulation call with their seams.
//!
//! Layout, inside the project:
//!
//! ```text
//! .verified/
//!   daemon.lock          held (Lock seam) while a daemon runs
//!   port                 `127.0.0.1:PORT\n`, written atomically
//!   v.toml               the rules file, yours to write (`rules_file`)
//!   actors/<id>.log      one record log per actor
//! ```

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use verified_core::{Outcome, Rejection};
use verified_domain::id::ActorId;
use verified_domain::{ActorName, Born, Record, Request, Tool};
use verified_wire::command::Outcome as WireOutcome;

use crate::limits::ServerLimits;
use crate::log::{FileLogStore, LogError, LogStore};
use crate::payload::{Parsed, ToolArgs};
use crate::routes;
use crate::seam::{Lock, LockError, LockGuard};
use crate::shell::{ActorHandle, Seams, Session, ShellConfig, ShellError};
use crate::shim::fs;

/// The daemon's directory inside a project.
pub const DIR: &str = ".verified";
/// The lock file, under [`DIR`].
pub const LOCK_FILE: &str = "daemon.lock";
/// The port file, under [`DIR`].
pub const PORT_FILE: &str = "port";
/// The actors' logs' directory, under [`DIR`].
pub const ACTORS_DIR: &str = "actors";

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
        session: ActorId,
        #[source]
        source: ShellError,
    },
    #[error("serving HTTP: {0}")]
    Serve(#[source] io::Error),
    /// A daemon task panicked (under the `sim` feature; the shipped daemon
    /// aborts instead). Crash-only: recovery on restart is the way back.
    #[error("a daemon task panicked")]
    Panicked,
}

fn io_error(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> DaemonError + use<> {
    let path = path.display().to_string();
    move |source| DaemonError::Io { what, path, source }
}

/// Why an actor was not created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("the daemon hosts its limit of {limit} actors")]
    Full { limit: usize },
    /// Its log exists, but its birth was refused (past the depth limit, or
    /// its log is full): it is no actor that can act.
    #[error("its birth was refused: {rejection:?}")]
    Unborn { rejection: Rejection },
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("the daemon is stopping")]
    Unavailable,
}

/// One actor the daemon hosts: its handle, and who it is once its `Born`
/// is durable in its log (`None`: a log a spawn made whose birth is not
/// durable yet, or never was; it is hosted, and born by its spawn).
#[derive(Clone)]
struct Entry {
    handle: ActorHandle,
    born: Option<Born>,
}

/// One actor, as the registry lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub actor: ActorId,
    pub born: Born,
}

type Created = Result<(WireOutcome, ActorHandle, bool), CreateError>;

enum RegistryInput {
    Create {
        born: Born,
        reply: oneshot::Sender<Created>,
    },
    Lookup {
        actor: ActorId,
        reply: oneshot::Sender<Option<ActorHandle>>,
    },
    List {
        reply: oneshot::Sender<Vec<Listed>>,
    },
    /// The actor's `Born` is durable.
    Born {
        born: Born,
    },
}

/// The actors a daemon hosts: actors are sessions, one log each. A handle
/// to the registry task, which never awaits a session.
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
    /// Creates the root actor `actor` (durably, born as `main`, offered
    /// every tool the shell runs) or finds it: `Exists` on a repeat.
    ///
    /// # Errors
    /// Too many actors, a log that could not be created, or shutdown.
    pub async fn create(&self, actor: ActorId) -> Result<(WireOutcome, ActorHandle), CreateError> {
        self.make(root(actor)).await
    }

    /// Creates the actor `born` names (durably), then delivers its birth,
    /// or finds it: `Exists` on a repeat, as a `Keyed` spawn run again
    /// after a crash finds it. One a crash left unborn is born before this
    /// returns.
    ///
    /// # Errors
    /// Too many actors, a log that could not be created, or shutdown.
    pub async fn spawn(&self, born: Born) -> Result<(WireOutcome, ActorHandle), CreateError> {
        self.make(born).await
    }

    async fn make(&self, born: Born) -> Result<(WireOutcome, ActorHandle), CreateError> {
        let (reply, answer) = oneshot::channel();
        self.inbox
            .send(RegistryInput::Create {
                born: born.clone(),
                reply,
            })
            .await
            .map_err(|_closed| CreateError::Unavailable)?;
        let (outcome, handle, unborn) = answer
            .await
            .map_err(|_dropped| CreateError::Unavailable)??;
        // A child is born now, as a step of every spawn (one already born
        // answers that it exists), and so is a root whose log a crash left
        // unborn. The caller waits on the actor, never the registry.
        if unborn || born.parent.is_some() {
            match handle
                .born(born.clone())
                .await
                .map_err(|_gone| CreateError::Unavailable)?
            {
                Outcome::Accepted | Outcome::Exists => {}
                Outcome::Rejected(rejection) => return Err(CreateError::Unborn { rejection }),
                Outcome::Stale => crate::defect("a birth is accepted, a repeat, or refused"),
            }
            // Listed from now on: its birth is durable.
            self.inbox
                .send(RegistryInput::Born { born })
                .await
                .map_err(|_closed| CreateError::Unavailable)?;
        }
        Ok((outcome, handle))
    }

    /// The actor, if the daemon hosts it.
    pub async fn lookup(&self, actor: ActorId) -> Option<ActorHandle> {
        let (reply, answer) = oneshot::channel();
        self.inbox
            .send(RegistryInput::Lookup { actor, reply })
            .await
            .ok()?;
        answer.await.ok().flatten()
    }

    /// Every born actor the daemon hosts, as it was born. An empty list
    /// when the daemon is stopping.
    pub async fn list(&self) -> Vec<Listed> {
        let (reply, answer) = oneshot::channel();
        if self
            .inbox
            .send(RegistryInput::List { reply })
            .await
            .is_err()
        {
            return Vec::new();
        }
        answer.await.unwrap_or_default()
    }
}

/// The children `records`' spawns name.
fn spawned(records: &[Record<Parsed>]) -> impl Iterator<Item = ActorId> + '_ {
    records
        .iter()
        .filter_map(|record| match record {
            Record::StepFinished { calls, .. } => Some(calls),
            Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        })
        .flatten()
        .filter_map(|call| match &call.request {
            Request::Run {
                args: ToolArgs::Spawn { child, .. },
            } => Some(*child),
            Request::Run { .. } | Request::Invalid { .. } => None,
        })
}

/// A root actor the user made: named `main`, offered every tool the shell
/// runs.
#[must_use]
pub fn root(actor: ActorId) -> Born {
    Born::root(
        actor,
        ActorName::parse("main").unwrap_or_else(|_name| crate::defect("`main` is an actor name")),
        vec![
            Tool::Read,
            Tool::Edit,
            Tool::Bash,
            Tool::Spawn,
            Tool::Send,
            Tool::Wake,
            Tool::Tasks,
            Tool::Cancel,
        ],
    )
}

struct RegistryTask {
    actors: BTreeMap<ActorId, Entry>,
    store: Arc<dyn LogStore>,
    seams: Seams,
    config: Arc<ShellConfig>,
    fatal: mpsc::Sender<DaemonError>,
    /// This registry, for the actors' deliveries.
    registry: Registry,
}

impl RegistryTask {
    fn spawn_session(
        &mut self,
        actor: ActorId,
        log: Box<dyn crate::log::Log>,
        records: Vec<Record<Parsed>>,
        born: Option<Born>,
    ) -> Result<ActorHandle, DaemonError> {
        let known = records.iter().find_map(|record| match record {
            Record::Born { born } => Some(born.clone()),
            Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::TurnStarted { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::CancelRequested { .. }
            | Record::TurnEnded { .. }
            | Record::EpochBumped { .. } => None,
        });
        let (handle, running) = Session::start(
            actor,
            log,
            records,
            born.clone(),
            self.registry.clone(),
            self.seams.clone(),
            Arc::clone(&self.config),
        )
        .map_err(|source| DaemonError::Shell {
            session: actor,
            source,
        })?;
        let fatal = self.fatal.clone();
        self.seams.tasks.spawn(async move {
            if let Err(source) = running.await {
                drop(fatal.try_send(DaemonError::Shell {
                    session: actor,
                    source,
                }));
            }
        });
        self.actors.insert(
            actor,
            Entry {
                handle: handle.clone(),
                born: known.or(born),
            },
        );
        Ok(handle)
    }

    async fn create(&mut self, born: Born) -> Created {
        let actor = born.actor;
        if let Some(entry) = self.actors.get(&actor) {
            return Ok((
                WireOutcome::Exists,
                entry.handle.clone(),
                entry.born.is_none(),
            ));
        }
        let limit = self.config.limits.actors;
        if self.actors.len() >= limit {
            return Err(CreateError::Full { limit });
        }
        let log = self.store.create(actor).await?;
        // A root's first resume records its birth, before it takes any
        // input. A child's log is created first and its spawn then
        // delivers its birth, as a step of its own (the order the model
        // checks): a crash between them leaves it unborn, and the spawn
        // run again births it.
        // A root is listed at once: its first resume records its birth
        // before it takes any input. A child is listed once its spawn has
        // made its birth durable (`RegistryInput::Born`).
        let root = born.parent.is_none();
        let handle = self
            .spawn_session(actor, log, Vec::new(), root.then(|| born.clone()))
            .unwrap_or_else(|error| {
                crate::defect(&format!("an empty log always replays: {error}"))
            });
        Ok((WireOutcome::Created, handle, !root))
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<RegistryInput>) {
        let shutdown = self.seams.tasks.shutdown().clone();
        loop {
            let input = tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                input = inbox.recv() => input,
            };
            match input {
                None => return,
                Some(RegistryInput::Create { born, reply }) => {
                    let created = self.create(born).await;
                    drop(reply.send(created));
                }
                Some(RegistryInput::Lookup { actor, reply }) => {
                    drop(reply.send(self.actors.get(&actor).map(|entry| entry.handle.clone())));
                }
                Some(RegistryInput::Born { born }) => {
                    if let Some(entry) = self.actors.get_mut(&born.actor) {
                        entry.born.get_or_insert(born);
                    }
                }
                Some(RegistryInput::List { reply }) => {
                    let listed = self
                        .actors
                        .iter()
                        .filter_map(|(actor, entry)| {
                            Some(Listed {
                                actor: *actor,
                                born: entry.born.clone()?,
                            })
                        })
                        .collect();
                    drop(reply.send(listed));
                }
            }
        }
    }
}

/// Recovers every actor in `store` and starts the registry.
///
/// # Errors
/// Limits under which some record would not fit the log, more session
/// logs than the store holds, or a log that does not open or replay.
pub async fn boot(
    store: Arc<dyn LogStore>,
    seams: Seams,
    config: Arc<ShellConfig>,
    fatal: mpsc::Sender<DaemonError>,
) -> Result<Registry, DaemonError> {
    config.limits.check()?;
    let (inbox, receiver) = mpsc::channel(config.limits.registry_inbox);
    let registry = Registry { inbox };
    let mut task = RegistryTask {
        actors: BTreeMap::new(),
        store: Arc::clone(&store),
        seams: seams.clone(),
        config,
        fatal,
        registry: registry.clone(),
    };
    let mut opened = Vec::new();
    for actor in store.list().await? {
        let (log, records) = store.open(actor).await?;
        opened.push((actor, log, records));
    }
    // A log a crash left unborn is a root's unless some log's spawn names
    // it: a child's log is made only after its parent's `StepFinished`,
    // which names its id, is durable. A root is born again as it was made;
    // a child is born when its parent's spawn runs again.
    let children: BTreeSet<ActorId> = opened
        .iter()
        .flat_map(|(_, _, records)| spawned(records))
        .collect();
    for (actor, log, records) in opened {
        let born = (!children.contains(&actor)).then(|| root(actor));
        task.spawn_session(actor, log, records, born)?;
    }
    seams.tasks.spawn(task.run(receiver));
    Ok(registry)
}

/// Serves the daemon on `listener` with sessions from `store` until the
/// daemon shuts down or a session fails.
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
    let stop = seams.tasks.shutdown().clone();
    let serving = axum::serve(listener, app).with_graceful_shutdown(async move {
        stop.cancelled().await;
    });
    // A failed or panicked task is fatal: the daemon stops serving (dropping
    // the server is the crash) and recovery on restart is the way back. The
    // crash is checked first: it also fires `shutdown`, which ends `serving`
    // gracefully, and must not read as a clean stop.
    let crashed = seams.tasks.crashed().clone();
    tokio::select! {
        biased;
        () = crashed.cancelled() => Err(DaemonError::Panicked),
        error = failed.recv() => {
            seams.tasks.shutdown().cancel();
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
    pub fn actors(&self) -> PathBuf {
        self.dir().join(ACTORS_DIR)
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
    durable_dir(&layout.actors()).await?;
    write_port_file(layout, addr).await?;
    let store = FileLogStore::new(
        layout.actors(),
        limits.log_line_bytes,
        limits.log_file_bytes,
        limits.actors,
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
