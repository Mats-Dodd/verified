//! The seams nondeterminism enters the client through.
//!
//! Time, entropy, the network and the terminal. Each is a dyn-compatible trait; the real
//! implementations live here and in [`crate::terminal`], and simulation
//! supplies its own.

use core::fmt;
use core::time::Duration;
use std::io;

use async_trait::async_trait;
use crossterm::event::Event;
use ratatui::Frame;
use tokio::io::{AsyncRead, AsyncWrite};
use verified_domain::id::{IdEntropy, UnixMillis};

use crate::shim;

/// Wall time for ids and sleeps for retry schedules.
#[async_trait]
pub trait Clock: Send + Sync + fmt::Debug {
    /// Milliseconds since the Unix epoch.
    fn now(&self) -> UnixMillis;

    /// Waits `duration`. Callers that must stop early race it against a
    /// cancellation token; sleeping holds nothing, so dropping it is safe.
    async fn sleep(&self, duration: Duration);
}

/// Random bytes for client-minted ids.
pub trait Entropy: Send + fmt::Debug {
    /// Ten fresh random bytes (the random part of a `UUIDv7`).
    fn id_entropy(&mut self) -> IdEntropy;
}

/// A byte stream to the daemon.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// Opens connections to the daemon.
#[async_trait]
pub trait Dialer: Send + Sync + fmt::Debug {
    /// The `host:port` requests name in their `host` header.
    fn authority(&self) -> &str;

    /// A new connection to the daemon.
    ///
    /// # Errors
    /// The daemon cannot be reached.
    async fn dial(&self) -> io::Result<Box<dyn Stream>>;
}

/// Why the terminal failed.
#[derive(Debug, thiserror::Error)]
#[error("terminal: {0}")]
pub struct TerminalError(#[from] pub io::Error);

/// Terminal input.
#[async_trait]
pub trait Input: Send + fmt::Debug {
    /// The next event, or `None` once input has ended.
    ///
    /// Must be cancel-safe: the runtime races it against shutdown, and an
    /// event the losing future had not yet returned must not be lost.
    ///
    /// # Errors
    /// Reading the terminal failed.
    async fn next_event(&mut self) -> Result<Option<Event>, TerminalError>;
}

/// Terminal output.
pub trait Screen: fmt::Debug {
    /// Draws one frame with `view`.
    ///
    /// # Errors
    /// Writing to the terminal failed.
    fn draw(&mut self, view: &mut dyn FnMut(&mut Frame<'_>)) -> Result<(), TerminalError>;
}

/// The real wall clock and tokio's timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the Clock seam's real implementation"
    )]
    fn now(&self) -> UnixMillis {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock is after 1970");
        let millis = u64::try_from(since_epoch.as_millis()).expect("milliseconds fit u64");
        UnixMillis::new(millis).expect("the system clock is before the year 10889")
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "this is the Clock seam's real implementation"
    )]
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Entropy from the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    #[expect(
        clippy::disallowed_methods,
        reason = "the Entropy seam's real impl is where randomness enters"
    )]
    fn id_entropy(&mut self) -> IdEntropy {
        let mut entropy = IdEntropy::default();
        getrandom::fill(&mut entropy).expect("the operating system provides entropy");
        entropy
    }
}

/// Dials the daemon over TCP: tokio's in production, turmoil's under the
/// `sim` feature (see [`crate::shim`]). `host` may be a name; turmoil
/// resolves host names to simulated hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpDialer {
    host: String,
    port: u16,
    authority: String,
}

impl TcpDialer {
    #[must_use]
    pub fn new(host: String, port: u16) -> Self {
        let authority = format!("{host}:{port}");
        Self {
            host,
            port,
            authority,
        }
    }
}

#[async_trait]
impl Dialer for TcpDialer {
    fn authority(&self) -> &str {
        &self.authority
    }

    async fn dial(&self) -> io::Result<Box<dyn Stream>> {
        let stream = shim::TcpStream::connect((self.host.as_str(), self.port)).await?;
        Ok(Box::new(stream))
    }
}
