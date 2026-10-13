//! Simulated time, for the daemon's and the TUI's Clock seams alike.

use async_trait::async_trait;
use verified_domain::Millis;
use verified_domain::id::UnixMillis;

/// Wall time inside a turmoil simulation: a fixed epoch plus the
/// simulation's elapsed time. Sleeps use tokio's timer, which turmoil
/// drives.
#[derive(Debug, Clone, Copy)]
pub struct SimClock {
    epoch: UnixMillis,
}

impl SimClock {
    /// A clock that reads `epoch` when the simulation starts.
    #[must_use]
    pub const fn new(epoch: UnixMillis) -> Self {
        Self { epoch }
    }
}

/// The simulated wall time on `clock`.
fn now(clock: SimClock) -> UnixMillis {
    let elapsed = turmoil::sim_elapsed().expect("SimClock runs inside a turmoil simulation");
    let elapsed = u64::try_from(elapsed.as_millis())
        .expect("a simulation runs for fewer than 2^64 milliseconds");
    let now = clock
        .epoch
        .get()
        .checked_add(elapsed)
        .expect("simulated time stays within 48-bit Unix milliseconds");
    UnixMillis::new(now).expect("simulated time stays within 48-bit Unix milliseconds")
}

/// Waits `delay` of simulated time.
#[expect(
    clippy::disallowed_methods,
    reason = "the simulated Clock seam; turmoil drives tokio's timer"
)]
async fn sleep(delay: Millis) {
    tokio::time::sleep(core::time::Duration::from_millis(delay.get())).await;
}

#[async_trait]
impl verified_server::seam::Clock for SimClock {
    fn now(&self) -> UnixMillis {
        now(*self)
    }

    async fn sleep(&self, delay: Millis) {
        sleep(delay).await;
    }
}

#[async_trait]
impl verified_tui::seam::Clock for SimClock {
    fn now(&self) -> UnixMillis {
        now(*self)
    }

    async fn sleep(&self, delay: Millis) {
        sleep(delay).await;
    }
}
