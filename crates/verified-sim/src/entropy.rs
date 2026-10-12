//! Seeded entropy for the daemon's and the TUI's Entropy seams.

use core::fmt;

use rand::{RngCore as _, SeedableRng as _};
use rand_chacha::ChaCha8Rng;
use verified_domain::id::IdEntropy;

/// The daemon's entropy: a seeded stream. The daemon's seam takes `&self`,
/// so the generator sits behind a lock.
pub struct DaemonEntropy {
    #[expect(
        clippy::disallowed_types,
        reason = "the daemon's Entropy seam takes &self; its session tasks are the writers, \
                  each draw advances one seeded stream, dropped with the host"
    )]
    rng: std::sync::Mutex<ChaCha8Rng>,
}

impl DaemonEntropy {
    #[must_use]
    #[expect(
        clippy::disallowed_types,
        reason = "the lock of the seeded stream described on the field"
    )]
    pub fn new(seed: u64) -> Self {
        Self {
            rng: std::sync::Mutex::new(ChaCha8Rng::seed_from_u64(seed)),
        }
    }
}

impl fmt::Debug for DaemonEntropy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonEntropy")
            .finish_non_exhaustive()
    }
}

impl verified_server::seam::Entropy for DaemonEntropy {
    fn fill(&self, out: &mut [u8]) {
        self.rng
            .lock()
            .expect("no entropy draw panics while holding the lock")
            .fill_bytes(out);
    }
}

/// The TUI's entropy: a seeded stream.
pub struct TuiEntropy {
    rng: ChaCha8Rng,
}

impl TuiEntropy {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed),
        }
    }
}

impl fmt::Debug for TuiEntropy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TuiEntropy").finish_non_exhaustive()
    }
}

impl verified_tui::seam::Entropy for TuiEntropy {
    fn id_entropy(&mut self) -> IdEntropy {
        let mut bytes = IdEntropy::default();
        self.rng.fill_bytes(&mut bytes);
        bytes
    }
}
