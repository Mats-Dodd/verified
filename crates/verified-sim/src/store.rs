//! A log store that tells the ledger what is durable.
//!
//! It wraps the daemon's real [`FileLogStore`] on turmoil's filesystem and
//! records every replay ([`Entry::Recovered`]) and every acknowledged group
//! commit ([`Entry::Acked`]): what the model's replay judges each boot,
//! and each act on the world, against.
//!
//! turmoil's filesystem answers at once, without yielding, so an append
//! would finish before any other task of the daemon ran. A real fsync takes
//! milliseconds while the runtime runs everything else; each append here
//! first waits [`SYNC_LATENCY`] of simulated time, so a task started before
//! its records were durable gets to act before they are.

use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use verified_domain::id::ActorId;
use verified_domain::{Millis, Record};
use verified_server::log::{FileLogStore, Log, LogError, LogStore};
use verified_server::payload::Parsed;
use verified_server::seam::Clock;

use crate::ledger::{Entry, Ledger};

/// How long each group commit takes to become durable.
pub const SYNC_LATENCY: Millis = Millis::new(2);

/// [`FileLogStore`], watched.
pub struct WatchedStore {
    inner: FileLogStore,
    ledger: Ledger,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for WatchedStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchedStore")
            .finish_non_exhaustive()
    }
}

impl WatchedStore {
    #[must_use]
    pub fn new(inner: FileLogStore, ledger: Ledger, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            ledger,
            clock,
        }
    }
}

#[async_trait]
impl LogStore for WatchedStore {
    async fn list(&self) -> Result<Vec<ActorId>, LogError> {
        self.inner.list().await
    }

    async fn create(&self, session: ActorId) -> Result<Box<dyn Log>, LogError> {
        let log = self.inner.create(session).await?;
        self.ledger.record(Entry::Created);
        Ok(Box::new(WatchedLog {
            inner: log,
            ledger: self.ledger.clone(),
            clock: Arc::clone(&self.clock),
        }))
    }

    async fn open(
        &self,
        session: ActorId,
    ) -> Result<(Box<dyn Log>, Vec<Record<Parsed>>), LogError> {
        let (log, records) = self.inner.open(session).await?;
        self.ledger.record(Entry::Recovered {
            records: records.clone(),
        });
        let watched = WatchedLog {
            inner: log,
            ledger: self.ledger.clone(),
            clock: Arc::clone(&self.clock),
        };
        Ok((Box::new(watched), records))
    }
}

/// One session's log, watched.
struct WatchedLog {
    inner: Box<dyn Log>,
    ledger: Ledger,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for WatchedLog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WatchedLog").finish_non_exhaustive()
    }
}

#[async_trait]
impl Log for WatchedLog {
    fn next_seq(&self) -> u64 {
        self.inner.next_seq()
    }

    async fn append(&mut self, records: &[Record<Parsed>], spare: u64) -> Result<(), LogError> {
        let first = self.inner.next_seq();
        self.clock.sleep(SYNC_LATENCY).await;
        self.inner.append(records, spare).await?;
        self.ledger.record(Entry::Acked {
            first,
            records: records.to_vec(),
        });
        Ok(())
    }
}
