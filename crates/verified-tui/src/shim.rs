//! The one place the `sim` feature swaps IO: tokio's network in production,
//! turmoil's simulated network (and its clock) under simulation.

/// Whether this build has the `sim` shim: turmoil instead of the OS. The
/// shipped binary refuses to compile when it is true.
pub const SIMULATED: bool = cfg!(feature = "sim");

#[cfg(not(feature = "sim"))]
pub use tokio::net::TcpStream;
#[cfg(feature = "sim")]
pub use turmoil::net::TcpStream;

#[cfg(feature = "sim")]
pub use sim::SimClock;

#[cfg(feature = "sim")]
mod sim {
    use core::time::Duration;

    use async_trait::async_trait;
    use verified_domain::id::UnixMillis;

    use crate::seam::Clock;

    /// Simulated time: a fixed epoch plus the simulation's elapsed time.
    /// Sleeps use tokio's timer, which turmoil drives.
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

    #[async_trait]
    impl Clock for SimClock {
        fn now(&self) -> UnixMillis {
            let elapsed =
                turmoil::sim_elapsed().expect("SimClock runs inside a turmoil simulation");
            let elapsed = u64::try_from(elapsed.as_millis()).expect("milliseconds fit u64");
            let now = self
                .epoch
                .get()
                .checked_add(elapsed)
                .expect("simulations run for less than u64::MAX milliseconds");
            UnixMillis::new(now).expect("simulated time stays within 48 bits")
        }

        #[expect(
            clippy::disallowed_methods,
            reason = "the simulated Clock seam; turmoil drives tokio's paused timer"
        )]
        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
    }
}
