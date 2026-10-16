//! Seeded entropy, for the daemon's and the TUI's Entropy seams alike.

use core::fmt;

use rand::{RngCore as _, SeedableRng as _};
use rand_chacha::ChaCha8Rng;

/// A seeded stream of random bytes: each host draws from its own seed, in
/// the order it draws. The seams take `&self`, so the generator sits behind
/// a lock.
pub struct SeededEntropy {
    #[expect(
        clippy::disallowed_types,
        reason = "the Entropy seams take &self; the host's tasks are the writers, \
                  each draw advances one seeded stream, dropped with the host"
    )]
    rng: std::sync::Mutex<ChaCha8Rng>,
}

impl SeededEntropy {
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

    /// Fills `out` with the stream's next bytes.
    fn draw(&self, out: &mut [u8]) {
        self.rng
            .lock()
            .expect("no entropy draw panics while holding the lock")
            .fill_bytes(out);
    }
}

impl fmt::Debug for SeededEntropy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SeededEntropy")
            .finish_non_exhaustive()
    }
}

impl verified_server::seam::Entropy for SeededEntropy {
    fn fill(&self, out: &mut [u8]) {
        self.draw(out);
    }
}

impl verified_tui::seam::Entropy for SeededEntropy {
    fn fill(&self, out: &mut [u8]) {
        self.draw(out);
    }
}
