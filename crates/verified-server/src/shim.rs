//! The one place IO is chosen.
//!
//! Each concern (network, filesystem, crashes, barriers) is one submodule;
//! the `sim` feature swaps it from tokio and the real OS to turmoil's
//! simulated network and filesystem.
//! Nothing outside this module names `tokio::net`, `tokio::fs` or turmoil.

/// TCP: the daemon's listener and the model client's connector.
pub mod net {
    use std::io;
    use std::net::SocketAddr;

    #[cfg(not(feature = "sim"))]
    pub use real::{Connector, Listener};
    #[cfg(feature = "sim")]
    pub use sim::{Connector, Listener, SimStream};

    /// Binds the daemon's listener.
    ///
    /// # Errors
    /// The address is in use or not local.
    pub async fn bind(addr: SocketAddr) -> io::Result<Listener> {
        Listener::bind(addr).await
    }

    #[cfg(not(feature = "sim"))]
    #[expect(
        clippy::disallowed_types,
        reason = "the shim is where the daemon's real listener is bound"
    )]
    mod real {
        use std::io;
        use std::net::SocketAddr;

        use hyper_util::client::legacy::connect::HttpConnector;

        /// A real TCP listener; `axum::serve` accepts it directly.
        #[derive(Debug)]
        pub struct Listener {
            listener: tokio::net::TcpListener,
        }

        impl Listener {
            pub(super) async fn bind(addr: SocketAddr) -> io::Result<Self> {
                tokio::net::TcpListener::bind(addr)
                    .await
                    .map(|listener| Self { listener })
            }
        }

        impl axum::serve::Listener for Listener {
            type Io = tokio::net::TcpStream;
            type Addr = SocketAddr;

            async fn accept(&mut self) -> (Self::Io, Self::Addr) {
                axum::serve::Listener::accept(&mut self.listener).await
            }

            fn local_addr(&self) -> io::Result<Self::Addr> {
                self.listener.local_addr()
            }
        }

        /// Plain-HTTP connector (TLS wraps it for Anthropic itself).
        pub type Connector = HttpConnector;
    }

    #[cfg(feature = "sim")]
    mod sim {
        use std::fmt;
        use std::io;
        use std::net::SocketAddr;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        use hyper::Uri;
        use hyper::rt::ReadBufCursor;
        use hyper_util::client::legacy::connect::Connected;
        use hyper_util::rt::TokioIo;
        use turmoil::net::{TcpListener, TcpStream};

        /// A turmoil TCP listener that `axum::serve` accepts connections from.
        pub struct Listener {
            listener: TcpListener,
        }

        impl Listener {
            pub(super) async fn bind(addr: SocketAddr) -> io::Result<Self> {
                TcpListener::bind(addr)
                    .await
                    .map(|listener| Self { listener })
            }
        }

        impl fmt::Debug for Listener {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct("Listener")
                    .field("local_addr", &self.listener.local_addr().ok())
                    .finish()
            }
        }

        impl axum::serve::Listener for Listener {
            type Io = TcpStream;
            type Addr = SocketAddr;

            async fn accept(&mut self) -> (Self::Io, Self::Addr) {
                self.listener
                    .accept()
                    .await
                    .expect("turmoil's accept never fails once the listener is bound")
            }

            fn local_addr(&self) -> io::Result<Self::Addr> {
                self.listener.local_addr()
            }
        }

        /// Dials hosts over turmoil's simulated network.
        #[derive(Clone, Copy, Debug, Default)]
        pub struct Connector;

        type Connecting = Pin<Box<dyn Future<Output = io::Result<SimStream>> + Send>>;

        impl tower_service::Service<Uri> for Connector {
            type Response = SimStream;
            type Error = io::Error;
            type Future = Connecting;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: Uri) -> Self::Future {
                let authority = req
                    .authority()
                    .map(|authority| authority.as_str().to_owned());
                Box::pin(async move {
                    let Some(authority) = authority else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("uri has no authority to dial: {req}"),
                        ));
                    };
                    let stream = TcpStream::connect(authority).await?;
                    Ok(SimStream {
                        stream: TokioIo::new(stream),
                    })
                })
            }
        }

        /// One simulated TCP connection, speaking hyper's IO traits.
        #[derive(Debug)]
        pub struct SimStream {
            stream: TokioIo<TcpStream>,
        }

        impl hyper::rt::Read for SimStream {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: ReadBufCursor<'_>,
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
            }
        }

        impl hyper::rt::Write for SimStream {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.get_mut().stream).poll_flush(cx)
            }

            fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
            }
        }

        impl hyper_util::client::legacy::connect::Connection for SimStream {
            fn connected(&self) -> Connected {
                Connected::new()
            }
        }
    }
}

/// Files: the log, the daemon directory and the project the tools touch.
///
/// Durability follows POSIX, which turmoil's filesystem models: a new file
/// is durable only once its data is synced *and* its directory is synced.
#[cfg_attr(
    not(feature = "sim"),
    expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "the shim is where the real filesystem is used; under sim it is turmoil's"
    )
)]
pub mod fs {
    use std::ffi::OsString;
    use std::io;
    use std::path::{Path, PathBuf};

    #[cfg(not(feature = "sim"))]
    use tokio::fs as backend;
    use tokio::io::AsyncWriteExt as _;
    #[cfg(feature = "sim")]
    use turmoil::fs::shim::tokio::fs as backend;

    /// An open file. An alias, not a re-export, so naming it outside the
    /// shim is not naming the backend's type.
    pub type File = backend::File;

    /// What a directory entry is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        File,
        Dir,
        Symlink,
        Other,
    }

    /// One directory entry.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Entry {
        pub name: OsString,
        pub kind: Kind,
    }

    const fn kind_of(is_file: bool, is_dir: bool, is_symlink: bool) -> Kind {
        match (is_symlink, is_dir, is_file) {
            (true, _, _) => Kind::Symlink,
            (false, true, _) => Kind::Dir,
            (false, false, true) => Kind::File,
            (false, false, false) => Kind::Other,
        }
    }

    /// # Errors
    /// Any OS error.
    pub async fn create_dir_all(path: &Path) -> io::Result<()> {
        backend::create_dir_all(path).await
    }

    /// Makes the entries of directory `path` durable.
    ///
    /// # Errors
    /// Any OS error.
    pub async fn sync_dir(path: &Path) -> io::Result<()> {
        #[cfg(not(feature = "sim"))]
        {
            File::open(path).await?.sync_all().await
        }
        #[cfg(feature = "sim")]
        {
            backend::sync_dir(path).await
        }
    }

    /// The kind and length of what `path` names, symlinks followed, or
    /// `None` when nothing is there (a dangling link included).
    ///
    /// # Errors
    /// Any OS error but not-found.
    pub async fn stat(path: &Path) -> io::Result<Option<(Kind, u64)>> {
        match backend::metadata(path).await {
            Ok(metadata) => Ok(Some((
                kind_of(
                    metadata.is_file(),
                    metadata.is_dir(),
                    metadata.file_type().is_symlink(),
                ),
                metadata.len(),
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The absolute path `path` names, every symlink resolved.
    ///
    /// # Errors
    /// Any OS error, including not-found.
    pub async fn canonicalize(path: &Path) -> io::Result<PathBuf> {
        backend::canonicalize(path).await
    }

    /// Reads all of `path`.
    ///
    /// # Errors
    /// Any OS error.
    pub async fn read(path: &Path) -> io::Result<Vec<u8>> {
        backend::read(path).await
    }

    /// Opens an existing file for appending.
    ///
    /// # Errors
    /// Any OS error, including not-found.
    pub async fn open_append(path: &Path) -> io::Result<File> {
        backend::OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .await
    }

    /// Creates a file that must not exist yet, for appending.
    ///
    /// # Errors
    /// Any OS error, including already-exists.
    pub async fn create_new(path: &Path) -> io::Result<File> {
        backend::OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)
            .await
    }

    /// Writes `bytes` to the end of `file` and makes them durable.
    ///
    /// # Errors
    /// Any OS error. After an error the file's tail is unknown.
    pub async fn append_synced(file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes).await?;
        file.flush().await?;
        crate::shim::barrier(crate::shim::Barrier::GroupWritten { bytes: bytes.len() }).await;
        file.sync_data().await
    }

    /// Cuts `file` to `len` bytes and makes that durable.
    ///
    /// # Errors
    /// Any OS error.
    pub async fn truncate(file: &File, len: u64) -> io::Result<()> {
        file.set_len(len).await?;
        file.sync_all().await
    }

    /// Creates or replaces `path` with `bytes`, synced.
    ///
    /// # Errors
    /// Any OS error.
    pub async fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut file = backend::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await
    }

    /// Gives `to` the permissions of `from`. A no-op under `sim`, so the
    /// simulation's schedule takes no extra IO step.
    ///
    /// # Errors
    /// Any OS error.
    #[cfg_attr(
        feature = "sim",
        expect(clippy::unused_async, reason = "async for the real build's IO")
    )]
    pub async fn copy_mode(from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(not(feature = "sim"))]
        {
            let permissions = backend::metadata(from).await?.permissions();
            backend::set_permissions(to, permissions).await
        }
        #[cfg(feature = "sim")]
        {
            let _: (&Path, &Path) = (from, to);
            Ok(())
        }
    }

    /// # Errors
    /// Any OS error.
    pub async fn rename(from: &Path, to: &Path) -> io::Result<()> {
        backend::rename(from, to).await
    }

    /// # Errors
    /// Any OS error.
    pub async fn remove_file(path: &Path) -> io::Result<()> {
        backend::remove_file(path).await
    }

    /// The entries of directory `path`, sorted by name, at most `max`.
    ///
    /// # Errors
    /// Any OS error, or `max` exceeded (`io::ErrorKind::FileTooLarge`).
    #[cfg_attr(
        not(feature = "sim"),
        expect(
            clippy::filetype_is_file,
            reason = "classifying entries: anything not a file, dir or symlink is Kind::Other"
        )
    )]
    pub async fn list(path: &Path, max: usize) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        #[cfg(not(feature = "sim"))]
        {
            let mut dir = backend::read_dir(path).await?;
            while let Some(entry) = dir.next_entry().await? {
                let kind = entry.file_type().await?;
                push_bounded(
                    &mut entries,
                    Entry {
                        name: entry.file_name(),
                        kind: kind_of(kind.is_file(), kind.is_dir(), kind.is_symlink()),
                    },
                    max,
                )?;
            }
        }
        #[cfg(feature = "sim")]
        {
            for entry in backend::read_dir(path).await? {
                let entry = entry?;
                let kind = entry.file_type()?;
                push_bounded(
                    &mut entries,
                    Entry {
                        name: entry.file_name(),
                        kind: kind_of(kind.is_file(), kind.is_dir(), kind.is_symlink()),
                    },
                    max,
                )?;
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn push_bounded(entries: &mut Vec<Entry>, entry: Entry, max: usize) -> io::Result<()> {
        if entries.len() >= max {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("more than {max} directory entries"),
            ));
        }
        entries.push(entry);
        Ok(())
    }
}

/// What a panicking daemon task does to the daemon (crash-only, Law 8).
pub mod crash {
    /// After a daemon task panicked and its [`crate::seam::Tasks`] crashed. The panic hook has
    /// already printed the panic and, where the daemon installed
    /// [`crate::seam::ProcessGroups::kill_on_panic`], killed the running
    /// commands.
    ///
    /// Real: aborts the process. Nothing unwinds and no other task takes
    /// another step; the next start recovers from the log.
    #[cfg(not(feature = "sim"))]
    pub(crate) fn task_panicked() {
        std::process::abort();
    }

    /// Sim: a process abort would end the whole test binary (every seed),
    /// so the daemon's host fails instead: [`crate::seam::Tasks::crashed`]
    /// has fired, every task stops at its next safe point and
    /// `daemon::serve` returns `DaemonError::Panicked`, which turmoil
    /// reports as the host's error and the simulation treats as a failed
    /// run.
    #[cfg(feature = "sim")]
    pub(crate) const fn task_panicked() {}
}

/// Points where a simulation may observe or suspend the daemon
/// (`turmoil::barriers`). Without the `sim` feature they compile to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barrier {
    /// A tool call's `Dispatched` record is durable and applied; the run
    /// with this id has not started.
    Dispatched(verified_domain::EffectId),
    /// A tool's run returned, its side effect (if any) done; its `ToolDone`
    /// has not been sent. `stopped`: the run reports it was stopped
    /// (`Interrupted`), so it acted only if it mutated before that.
    ToolRan {
        id: verified_domain::EffectId,
        stopped: bool,
    },
    /// A group commit's bytes are written but not yet synced.
    GroupWritten { bytes: usize },
}

/// Triggers `point` for a simulation's barriers. Under `sim` it is first
/// [`observe`]d, so a barrier that suspends here is still seen to be passed.
pub async fn barrier(point: Barrier) {
    #[cfg(feature = "sim")]
    {
        observe(Observation::Passed(point));
        turmoil::barriers::trigger(point).await;
    }
    #[cfg(not(feature = "sim"))]
    {
        let _: Barrier = point;
    }
}

/// What a simulation sees the shell do, beyond the log and the world.
///
/// Each input as it enters the core, with the records decided for it, and
/// each [`Barrier`] passed. Only under `sim`: the shipped daemon neither
/// builds nor clones any of it.
#[cfg(feature = "sim")]
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// The shell resumed at boot and `resume` decided these records.
    Resumed {
        records: Vec<verified_domain::Record<crate::payload::Parsed>>,
    },
    /// `event` entered `decide`, which decided these records. The shell
    /// handles one input at a time, so this is the event's place in the
    /// core's order.
    Decided {
        event: verified_core::Event<crate::payload::Parsed>,
        records: Vec<verified_domain::Record<crate::payload::Parsed>>,
    },
    /// The log refused the decision being persisted (the last one
    /// `Resumed` or `Decided` reported): none of its records were written
    /// or applied, and none of its effects start.
    Refused,
    /// A barrier was reached.
    Passed(Barrier),
}

/// Reports `observation` to a simulation that watches for it
/// (`turmoil::barriers`, observe-only: it never suspends the shell).
#[cfg(feature = "sim")]
pub fn observe(observation: Observation) {
    turmoil::barriers::trigger_noop(observation);
}
