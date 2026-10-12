//! A bounded, explicit event log for comparing simulation runs.
//!
//! Hosts record what they observed, stamped with simulated time. Running the
//! same seed twice must produce identical traces; any difference is
//! nondeterminism leaking past a seam.

use std::{
    sync::mpsc::{self, Receiver, SyncSender},
    time::Duration,
};

/// One observation made by a simulated host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceEvent {
    /// The host that made the observation.
    pub host: &'static str,
    /// Simulated time since the simulation started.
    pub at: Duration,
    /// What the host observed.
    pub observation: String,
}

/// Creates a trace that holds at most `capacity` events.
#[must_use]
pub fn bounded(capacity: usize) -> (TraceSink, TraceReader) {
    let (sender, receiver) = mpsc::sync_channel(capacity);
    (TraceSink { sender }, TraceReader { receiver })
}

/// The writing end of a trace, cloned into each host.
#[derive(Clone, Debug)]
pub struct TraceSink {
    sender: SyncSender<TraceEvent>,
}

impl TraceSink {
    /// Records an observation at the current simulated time.
    ///
    /// # Panics
    ///
    /// Panics when called outside a simulation or when the trace is full:
    /// the capacity is chosen up front, so overflowing it is a harness defect.
    pub fn record(&self, host: &'static str, observation: String) {
        let at = turmoil::sim_elapsed().expect("traces are recorded inside a simulation");
        self.sender
            .try_send(TraceEvent {
                host,
                at,
                observation,
            })
            .expect("trace capacity is fixed up front and must cover the whole run");
    }
}

/// The reading end of a trace.
#[derive(Debug)]
pub struct TraceReader {
    receiver: Receiver<TraceEvent>,
}

impl TraceReader {
    /// Takes every event recorded so far, in recording order.
    #[must_use]
    pub fn drain(&self) -> Vec<TraceEvent> {
        self.receiver.try_iter().collect()
    }
}
