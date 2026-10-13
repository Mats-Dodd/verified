//! A world to simulate, drawn from a seed.
//!
//! The scenario is pure data: what the user types and answers, which
//! faults hit the world and when, and how the disk tears. Same seed, same
//! scenario.

use core::time::Duration;

use rand::{Rng as _, SeedableRng as _};
use rand_chacha::ChaCha8Rng;
use verified_wire::command::Verdict;

/// The files the project starts with, and their text.
pub const FILES: [(&str, &str); 2] = [
    ("notes.txt", "alpha beta gamma delta"),
    ("other.txt", "one two three"),
];

/// The words of `notes.txt` an `edit` may replace.
pub(crate) const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

/// When the user presses Esc during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelAt {
    /// Once the reply has started streaming text.
    Streaming,
    /// Once a tool is running.
    ToolRunning,
    /// Instead of answering an approval prompt.
    Prompt,
}

/// One message the user sends, and how they behave during its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPlan {
    pub text: String,
    /// The answers to this turn's approval prompts, in order (cycled).
    pub verdicts: Vec<Verdict>,
    pub cancel: Option<CancelAt>,
}

/// Something that goes wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The daemon host crashes (losing everything not synced) and stays down.
    Crash { down: Duration },
    /// The TUI and the daemon cannot reach each other, then the link heals.
    Partition { lasting: Duration },
    /// The daemon and Anthropic cannot reach each other, then the link
    /// heals: the model client's timeouts and the core's retries ride it out,
    /// or the turn fails.
    ModelPartition { lasting: Duration },
}

/// Where in the daemon a crash may be aimed (a `turmoil` barrier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aim {
    /// After a tool call's `Dispatched` is durable, before the tool starts.
    Dispatched,
    /// After a tool's side effect, before its result reaches the core.
    ToolRan,
    /// In a group commit: written, not yet synced.
    GroupWritten,
}

/// A crash at the `occurrence`th time the daemon reaches `aim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimedCrash {
    pub aim: Aim,
    pub occurrence: u32,
    pub down: Duration,
}

/// A partial-order constraint on the daemon, kept with a barrier.
///
/// It orders a few points of the run and leaves the rest of its schedule
/// to the seed. A directed scenario ([`crate::directed`]) states its target's
/// essential ordering as holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// The first tool run to act waits, before it reports, until a second
    /// has acted too: two runs act before either reports.
    RanTogether,
}

/// Everything one simulated run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub seed: u64,
    pub turns: Vec<TurnPlan>,
    /// Faults by the simulated time they strike, in order.
    pub faults: Vec<(Duration, Fault)>,
    pub aimed: Option<AimedCrash>,
    /// Orderings the run must keep (a seed's scenario has none).
    pub holds: Vec<Hold>,
    /// Torn-write granularity of the daemon's disk (`None`: atomic writes).
    pub block_size: Option<u64>,
    pub max_latency: Duration,
}

fn verdicts(rng: &mut ChaCha8Rng) -> Vec<Verdict> {
    (0..4)
        .map(|_| {
            if rng.random_bool(0.75) {
                Verdict::Grant
            } else {
                Verdict::Deny
            }
        })
        .collect()
}

fn turn(rng: &mut ChaCha8Rng, seed: u64, index: usize) -> TurnPlan {
    let word = WORDS
        .get(rng.random_range(0..WORDS.len()))
        .copied()
        .unwrap_or("alpha");
    let other = index
        .wrapping_add(1)
        .checked_rem(WORDS.len())
        .and_then(|at| WORDS.get(at))
        .copied()
        .unwrap_or("beta");
    let fresh = format!("w{index}s{seed:x}");
    let (text, cancel) = match rng.random_range(0..14u32) {
        0..=2 => (format!("hello {index}"), None),
        3 => (format!("slow {index}"), Some(CancelAt::Streaming)),
        4 => (format!("slow {index}"), None),
        5 => (format!("fail {index}"), None),
        6 => (format!("overloaded {index}"), None),
        7 => ("read notes.txt".to_owned(), None),
        8 => (format!("edit notes.txt {word} {fresh}"), None),
        9 => (format!("bash touch t{index}"), None),
        10 => ("bash sleep 3000".to_owned(), Some(CancelAt::ToolRunning)),
        11 => ("read notes.txt ; read other.txt".to_owned(), None),
        // Approved calls all run at once: two edits of one file run in one
        // step and may race, and a command runs beside them.
        12 => (
            format!(
                "bash touch m{index} ; edit notes.txt {word} {fresh} ; \
                 edit notes.txt {other} {fresh}b ; read other.txt"
            ),
            None,
        ),
        _ => ("call nope {}".to_owned(), None),
    };
    let cancel = match cancel {
        Some(at) if rng.random_bool(0.6) => Some(at),
        None if rng.random_bool(0.1) => Some(CancelAt::Prompt),
        Some(_) | None => None,
    };
    TurnPlan {
        text,
        verdicts: verdicts(rng),
        cancel,
    }
}

impl Scenario {
    /// The scenario of `seed`.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let turns = (0..rng.random_range(2..=5usize))
            .map(|index| turn(&mut rng, seed, index))
            .collect();
        let mut faults: Vec<(Duration, Fault)> = (0..rng.random_range(0..=3u32))
            .map(|_| {
                let at = Duration::from_millis(rng.random_range(5..3_000));
                // Mostly blips the client's retries ride out; sometimes an
                // outage that outlasts them, so the user has to resend and
                // reconnect by hand.
                let longest = if rng.random_bool(0.2) { 60_000 } else { 3_000 };
                let fault = match rng.random_range(0..3u32) {
                    0 => Fault::Crash {
                        down: Duration::from_millis(rng.random_range(0..longest)),
                    },
                    1 => Fault::Partition {
                        lasting: Duration::from_millis(rng.random_range(50..longest)),
                    },
                    _ => Fault::ModelPartition {
                        lasting: Duration::from_millis(
                            rng.random_range(50..longest.saturating_mul(2)),
                        ),
                    },
                };
                (at, fault)
            })
            .collect();
        // A daemon down from the very start: the TUI's create exhausts its
        // retries and the user retries by hand (^r).
        if rng.random_bool(0.1) {
            faults.push((
                Duration::ZERO,
                Fault::Crash {
                    down: Duration::from_millis(rng.random_range(4_000..20_000)),
                },
            ));
        }
        // A TUI link down for longer than the follower's whole reconnect
        // schedule (about 132s): the TUI stops following (Down) and the user
        // reconnects by hand.
        if rng.random_bool(0.08) {
            faults.push((
                Duration::from_millis(rng.random_range(5..3_000)),
                Fault::Partition {
                    lasting: Duration::from_millis(rng.random_range(170_000..200_000)),
                },
            ));
        }
        faults.sort_by_key(|(at, _)| *at);
        let aimed = rng.random_bool(0.5).then(|| AimedCrash {
            // A crash before a tool starts is the one that leaves an
            // Unsafe call Unknown: aim there twice as often.
            aim: match rng.random_range(0..4u32) {
                1 => Aim::ToolRan,
                2 => Aim::GroupWritten,
                _ => Aim::Dispatched,
            },
            occurrence: rng.random_range(0..4),
            down: Duration::from_millis(rng.random_range(0..2_000)),
        });
        let block_size = match rng.random_range(0..4u32) {
            0 => None,
            1 => Some(16),
            2 => Some(64),
            _ => Some(512),
        };
        Self {
            seed,
            turns,
            faults,
            aimed,
            holds: Vec::new(),
            block_size,
            max_latency: Duration::from_millis(rng.random_range(1..=40)),
        }
    }
}
