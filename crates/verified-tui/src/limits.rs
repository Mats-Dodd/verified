//! Every bound the client runs under, parsed once at startup and passed in.

use core::time::Duration;

use verified_wire::WireLimits;

/// A retry schedule as data: the delay before each retry, doubling from
/// `first` up to `ceiling`, for at most `attempts` retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub first: Duration,
    pub ceiling: Duration,
    pub attempts: u32,
}

impl Schedule {
    /// The delay before retry number `attempt` (counting from zero), or
    /// `None` once the schedule is spent.
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Option<Duration> {
        if attempt >= self.attempts {
            return None;
        }
        let factor = 2u32.saturating_pow(attempt);
        Some(self.first.saturating_mul(factor).min(self.ceiling))
    }
}

/// Bounds for the TUI: wire decoding, the inbox, effects in flight and
/// retry schedules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bounds for decoding replies and the stream, and for the transcript.
    pub wire: WireLimits,
    /// Capacity of the one inbox; also the most messages folded into one redraw.
    pub inbox: usize,
    /// Most effect tasks in flight at once (commands, the follower, input).
    pub tasks: usize,
    /// Retries of a command whose delivery failed in transit. Every retry
    /// sends the same body, so the same client-minted id.
    pub command_retry: Schedule,
    /// Reconnects of the stream after consecutive failures.
    pub reconnect: Schedule,
    /// The longest one command exchange (connect, request, whole reply) or
    /// the opening of a stream read may take before it counts as a failed
    /// delivery and is retried.
    pub exchange_timeout: Duration,
    /// The longest a live stream may stay silent (the daemon sends a
    /// keep-alive well within it) before the connection counts as dead and
    /// the follower reconnects from its last offset.
    pub stream_idle: Duration,
    /// The tallest the input box grows, in lines.
    pub input_lines: u16,
}

impl Limits {
    /// Bounds for development and tests; production takes them from config.
    pub const DEFAULT: Self = Self {
        wire: WireLimits::DEFAULT,
        inbox: 256,
        tasks: 8,
        command_retry: Schedule {
            first: Duration::from_millis(100),
            ceiling: Duration::from_secs(2),
            attempts: 5,
        },
        reconnect: Schedule {
            first: Duration::from_millis(250),
            ceiling: Duration::from_secs(5),
            attempts: 30,
        },
        exchange_timeout: Duration::from_secs(10),
        stream_idle: Duration::from_secs(35),
        input_lines: 8,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_double_up_to_the_ceiling_then_stop() {
        let schedule = Schedule {
            first: Duration::from_millis(250),
            ceiling: Duration::from_secs(1),
            attempts: 4,
        };
        let delays: Vec<_> = (0..5).map(|attempt| schedule.delay(attempt)).collect();
        assert_eq!(
            delays,
            [
                Some(Duration::from_millis(250)),
                Some(Duration::from_millis(500)),
                Some(Duration::from_secs(1)),
                Some(Duration::from_secs(1)),
                None,
            ]
        );
    }

    #[test]
    fn huge_attempt_numbers_saturate() {
        let schedule = Schedule {
            first: Duration::from_secs(1),
            ceiling: Duration::from_secs(5),
            attempts: u32::MAX,
        };
        assert_eq!(schedule.delay(u32::MAX - 1), Some(Duration::from_secs(5)));
    }
}
