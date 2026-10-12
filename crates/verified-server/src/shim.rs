//! The one place IO is chosen.
//!
//! Each concern (network, filesystem, simulated
//! time, barriers) is one submodule; the `sim` feature swaps it from tokio
//! and the real OS to turmoil's simulated network, filesystem and clock.
//! Nothing outside this module names `tokio::net`, `tokio::fs` or turmoil.

/// Whether this build has the `sim` shims: turmoil instead of the OS. The
/// shipped binary refuses to compile when it is true.
pub const SIMULATED: bool = cfg!(feature = "sim");

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
pub mod fs {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;

    #[cfg(not(feature = "sim"))]
    use tokio::fs as backend;
    use tokio::io::AsyncWriteExt as _;
    #[cfg(feature = "sim")]
    use turmoil::fs::shim::tokio::fs as backend;

    pub use backend::File;

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

    /// The kind and length of `path`, or `None` when nothing is there.
    ///
    /// # Errors
    /// Any OS error but not-found.
    pub async fn stat(path: &Path) -> io::Result<Option<(Kind, u64)>> {
        match backend::symlink_metadata(path).await {
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

/// Simulated wall time: turmoil's elapsed time from a fixed epoch.
#[cfg(feature = "sim")]
pub mod time {
    use async_trait::async_trait;
    use verified_domain::Millis;
    use verified_domain::id::UnixMillis;

    use crate::seam::Clock;

    /// Wall time inside a turmoil simulation, from `epoch`.
    #[derive(Debug, Clone, Copy)]
    pub struct SimClock {
        epoch: UnixMillis,
    }

    impl SimClock {
        #[must_use]
        pub const fn new(epoch: UnixMillis) -> Self {
            Self { epoch }
        }
    }

    #[async_trait]
    impl Clock for SimClock {
        fn now(&self) -> UnixMillis {
            let elapsed = u64::try_from(turmoil::elapsed().as_millis())
                .expect("a simulation runs for fewer than 2^64 milliseconds");
            let now = self
                .epoch
                .get()
                .checked_add(elapsed)
                .expect("simulated time stays within 48-bit Unix milliseconds");
            UnixMillis::new(now).expect("simulated time stays within 48-bit Unix milliseconds")
        }

        #[expect(
            clippy::disallowed_methods,
            reason = "the Clock seam's sim impl; turmoil drives tokio's timer"
        )]
        async fn sleep(&self, delay: Millis) {
            tokio::time::sleep(core::time::Duration::from_millis(delay.get())).await;
        }
    }
}

/// What a panicking world task does to the daemon (crash-only, Law 8).
pub mod crash {
    use core::any::Any;

    use crate::seam::World;

    /// The panic hook has already printed the panic's message and location.
    ///
    /// Real: kills the running commands' process groups, then aborts the
    /// process. Nothing unwinds and no other task takes another step; the
    /// next start recovers from the log.
    #[cfg(not(feature = "sim"))]
    pub(crate) fn task_panicked(world: &World, payload: &(dyn Any + Send)) {
        let _: &(dyn Any + Send) = payload;
        // Nothing is dropped by an abort: kill every running command's
        // process group first, grandchildren included.
        world.groups().kill_all();
        std::process::abort();
    }

    /// Sim: a process abort would end the whole test binary (every seed),
    /// so the daemon's host fails instead: [`World::crashed`] fires, every
    /// task stops at its next safe point and `daemon::serve` returns
    /// `DaemonError::Panicked`, which turmoil reports as the host's error
    /// and the simulation treats as a failed run.
    #[cfg(feature = "sim")]
    pub(crate) fn task_panicked(world: &World, payload: &(dyn Any + Send)) {
        let _: &(dyn Any + Send) = payload;
        world.crash();
    }
}

/// Points where a simulation may observe or suspend the daemon
/// (`turmoil::barriers`). Without the `sim` feature they compile to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barrier {
    /// A tool call's `Dispatched` record is durable and applied; the tool has
    /// not started. `seq` is the effect id's sequence number.
    Dispatched { epoch: u64, seq: u64 },
    /// A tool's side effect happened; its `ToolDone` has not been sent.
    ToolRan { epoch: u64, seq: u64 },
    /// A group commit's bytes are written but not yet synced.
    GroupWritten { bytes: usize },
}

/// Triggers `point` for a simulation's barriers.
pub async fn barrier(point: Barrier) {
    #[cfg(feature = "sim")]
    {
        turmoil::barriers::trigger(point).await;
    }
    #[cfg(not(feature = "sim"))]
    {
        let _: Barrier = point;
    }
}
