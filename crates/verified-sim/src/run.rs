//! One simulated run: build the world from a seed, inject its faults, let the
//! scripted user finish, crash the daemon one last time, judge the world and
//! replay the run through the model.

use core::time::Duration;
use std::collections::VecDeque;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use turmoil::barriers::{Barrier as SimBarrier, Reaction, Triggered};
use verified_model::{Mismatch, Observed, Replayed};
use verified_server::shell::Planted;
use verified_server::shim::{Barrier, Observation};

use crate::check::{self, Violation};
use crate::driver;
use crate::hosts::{self, ANTHROPIC, DAEMON, TUI};
use crate::ledger::{self, Entry, Ledger, Stamped};
use crate::model;
use crate::scenario::{Aim, AimedCrash, Fault, Scenario};

/// The seeds the simulation's tests run, each for a path it reaches
/// (`tests/dst.rs`), and the seeds `processes` traces by default.
pub const FIXED: [u64; 14] = [0, 1, 2, 3, 4, 5, 6, 7, 37, 50, 51, 53, 342, 818];

/// The most entries one run's ledger holds.
const LEDGER_CAPACITY: usize = 100_000;
/// The longest a run may take, in simulated time, before it counts as stuck.
const RUN_LIMIT: Duration = Duration::from_mins(15);
/// The most steps the daemon gets to recover after the final crash.
const RECOVERY_STEPS: usize = 20_000;

/// What a finished run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub scenario: Scenario,
    /// The ledger: what happened in the world, in order.
    pub entries: Vec<Stamped>,
    /// Every world invariant broken.
    pub violations: Vec<Violation>,
    /// The run as the model replays it.
    pub observed: Vec<Observed>,
    /// What the replay walked through, or why the run is no path of the
    /// model.
    pub replayed: Result<Replayed, Mismatch>,
}

impl Outcome {
    /// The run's determinism trace ([`ledger::trace`]).
    #[must_use]
    pub fn trace(&self) -> Vec<String> {
        ledger::trace(&self.entries)
    }

    /// What the replay walked through, if the run broke nothing: no world
    /// invariant, and the model followed it.
    ///
    /// # Errors
    /// The run broke a world invariant, or left the model.
    pub fn verdict(self) -> Result<Replayed, Broken> {
        match self.replayed {
            Ok(replayed) if self.violations.is_empty() => Ok(replayed),
            Ok(_) => Err(Broken {
                seed: self.scenario.seed,
                violations: self.violations,
                mismatch: None,
            }),
            Err(mismatch) => Err(Broken {
                seed: self.scenario.seed,
                violations: self.violations,
                mismatch: Some(mismatch),
            }),
        }
    }
}

/// A run that broke something: the world invariants it broke, and why the
/// model could not follow it, if it could not.
#[derive(Debug, thiserror::Error)]
#[error(
    "seed {seed} broke {violations:#?}{}",
    mismatch.as_ref().map_or_else(String::new, |mismatch| format!("\n{mismatch}"))
)]
pub struct Broken {
    pub seed: u64,
    pub violations: Vec<Violation>,
    pub mismatch: Option<Mismatch>,
}

/// Why a run did not finish, or a seed's two runs differ.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("seed {seed}: the simulation failed after {elapsed:?}: {reason}")]
    Failed {
        seed: u64,
        elapsed: Duration,
        reason: String,
        /// The ledger up to the failure.
        trace: Vec<String>,
    },
    #[error(
        "seed {seed}: two runs diverged at trace line {line}:\n  first:  {}\n  second: {}",
        trace.get(*line).map_or("<end>", String::as_str),
        other.as_deref().unwrap_or("<end>")
    )]
    Diverged {
        seed: u64,
        line: usize,
        /// The first run's trace.
        trace: Vec<String>,
        /// The second run's line there.
        other: Option<String>,
    },
}

impl RunError {
    /// The trace up to the failure, or the first run's.
    #[must_use]
    pub fn trace(&self) -> &[String] {
        match self {
            Self::Failed { trace, .. } | Self::Diverged { trace, .. } => trace,
        }
    }
}

/// What the harness does at a simulated time: a fault strikes, or one it
/// struck ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Strike(Fault),
    Bounce,
    Repair,
    RepairModel,
}

/// The crash aimed at a barrier, while it waits to fire.
struct Aimed {
    plan: AimedCrash,
    barrier: SimBarrier<Barrier>,
    passed: u32,
}

/// [`Scenario::ran_together`] until two runs have acted: the runs caught
/// acting, each waiting before it reports.
struct Together {
    barrier: SimBarrier<Barrier>,
    waiting: Vec<Triggered<Barrier>>,
}

/// The faults of a run, applied as simulated time passes.
struct Faults {
    /// Every step still to take, by time (at one time, as scheduled).
    steps: VecDeque<(Duration, Step)>,
    aimed: Option<Aimed>,
    together: Option<Together>,
    daemon_up: bool,
    /// Partitions of the TUI's link in force (they may overlap).
    partitioned: usize,
    /// Partitions of the daemon's link to Anthropic in force.
    model_partitioned: usize,
}

/// Polls `future` once: a barrier's channel answers without a runtime.
fn poll_once<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

const fn aimed_at(aim: Aim, point: &Barrier) -> bool {
    matches!(
        (aim, point),
        (Aim::Dispatched, Barrier::Dispatched(_))
            | (Aim::ToolRan, Barrier::ToolRan { .. })
            | (Aim::GroupWritten, Barrier::GroupWritten { .. })
    )
}

impl Faults {
    fn new(scenario: &Scenario) -> Self {
        let aimed = scenario.aimed.map(|plan| Aimed {
            plan,
            barrier: SimBarrier::build(Reaction::Suspend, move |point: &Barrier| {
                aimed_at(plan.aim, point)
            }),
            passed: 0,
        });
        let together = scenario.ran_together.then(|| Together {
            barrier: SimBarrier::build(Reaction::Suspend, |point: &Barrier| {
                matches!(point, Barrier::ToolRan { .. })
            }),
            waiting: Vec::new(),
        });
        let mut faults = Self {
            steps: VecDeque::new(),
            aimed,
            together,
            daemon_up: true,
            partitioned: 0,
            model_partitioned: 0,
        };
        for (at, fault) in &scenario.faults {
            faults.schedule(*at, Step::Strike(*fault));
        }
        faults
    }

    /// Schedules `step` at `at`, after every step already due by then.
    fn schedule(&mut self, at: Duration, step: Step) {
        let index = self.steps.partition_point(|(other, _)| *other <= at);
        self.steps.insert(index, (at, step));
    }

    const fn links(&self) -> Links {
        Links {
            tui: self.partitioned > 0,
            model: self.model_partitioned > 0,
        }
    }

    fn crash(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger, why: String, down: Duration) {
        let now = sim.elapsed();
        crash_daemon(sim, self.links());
        self.daemon_up = false;
        ledger.record_at(now, Entry::Fault(why));
        self.schedule(now.saturating_add(down), Step::Bounce);
    }

    fn take(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger, step: Step) {
        let now = sim.elapsed();
        match step {
            Step::Strike(Fault::Crash { down }) => {
                if self.daemon_up {
                    self.crash(sim, ledger, format!("daemon crashes for {down:?}"), down);
                }
            }
            Step::Strike(Fault::Partition { lasting }) => {
                if self.partitioned == 0 {
                    sim.partition(TUI, DAEMON);
                }
                self.partitioned = self.partitioned.saturating_add(1);
                ledger.record_at(
                    now,
                    Entry::Fault(format!("the TUI link partitions for {lasting:?}")),
                );
                self.schedule(now.saturating_add(lasting), Step::Repair);
            }
            Step::Strike(Fault::ModelPartition { lasting }) => {
                if self.model_partitioned == 0 {
                    sim.partition(DAEMON, ANTHROPIC);
                }
                self.model_partitioned = self.model_partitioned.saturating_add(1);
                ledger.record_at(
                    now,
                    Entry::Fault(format!("the model link partitions for {lasting:?}")),
                );
                self.schedule(now.saturating_add(lasting), Step::RepairModel);
            }
            Step::Bounce => {
                if !self.daemon_up {
                    sim.bounce(DAEMON);
                    self.daemon_up = true;
                    ledger.record_at(now, Entry::Fault("daemon restarts".to_owned()));
                }
            }
            Step::Repair => {
                self.partitioned = self.partitioned.saturating_sub(1);
                if self.partitioned == 0 {
                    sim.repair(TUI, DAEMON);
                    ledger.record_at(now, Entry::Fault("the TUI link heals".to_owned()));
                }
            }
            Step::RepairModel => {
                self.model_partitioned = self.model_partitioned.saturating_sub(1);
                if self.model_partitioned == 0 {
                    sim.repair(DAEMON, ANTHROPIC);
                    ledger.record_at(now, Entry::Fault("the model link heals".to_owned()));
                }
            }
        }
    }

    /// Takes every step due at the simulation's current time.
    fn tick(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger) {
        let now = sim.elapsed();
        while let Some((at, step)) = self.steps.front().copied()
            && at <= now
        {
            self.steps.pop_front();
            self.take(sim, ledger, step);
        }
        self.aim(sim, ledger);
        self.hold(sim, ledger);
    }

    /// Keeps each run caught acting waiting until two have acted, then
    /// releases both and forgets the barrier. A second run that never comes
    /// holds the first until the run's time limit fails it.
    fn hold(&mut self, sim: &turmoil::Sim<'_>, ledger: &Ledger) {
        let now = sim.elapsed();
        let Some(together) = self.together.as_mut() else {
            return;
        };
        while let Some(hit) = poll_once(together.barrier.wait()).flatten() {
            ledger.record_at(now, Entry::Fault(format!("held at {:?}", *hit)));
            together.waiting.push(hit);
        }
        if together.waiting.len() >= 2 {
            ledger.record_at(now, Entry::Fault("two runs acted together".to_owned()));
            self.together = None;
        }
    }

    /// Releases each barrier hit until the aimed one, then crashes there.
    fn aim(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger) {
        loop {
            let Some(aimed) = self.aimed.as_mut() else {
                return;
            };
            let Some(hit) = poll_once(aimed.barrier.wait()).flatten() else {
                return;
            };
            if aimed.passed < aimed.plan.occurrence || !self.daemon_up {
                aimed.passed = aimed.passed.saturating_add(1);
                drop(hit);
                continue;
            }
            let plan = aimed.plan;
            // Crash while suspended at the barrier, then forget the barrier
            // so it does not catch the restarted daemon.
            self.crash(
                sim,
                ledger,
                format!("daemon crashes at {:?} for {:?}", *hit, plan.down),
                plan.down,
            );
            drop(hit);
            self.aimed = None;
        }
    }

    /// Ends every fault still in force, and strikes no more.
    fn heal(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger) {
        self.aimed = None;
        self.together = None;
        for (_, step) in core::mem::take(&mut self.steps) {
            match step {
                Step::Strike(_) => {}
                Step::Bounce | Step::Repair | Step::RepairModel => self.take(sim, ledger, step),
            }
        }
    }
}

/// Which of the daemon's links a fault holds partitioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Links {
    tui: bool,
    model: bool,
}

/// Crashes the daemon like a machine losing power: its peers hear nothing.
///
/// Turmoil drops a crashed host's runtime, and each socket that drops sends
/// a close that draws its latency from the network's RNG. Tokio drops tasks
/// in an order set by process-global task ids, so in one process two runs of
/// a seed would draw in different orders. Cutting the daemon's links for the
/// crash discards those closes before they draw a latency, which keeps the
/// run a function of its seed alone.
/// Links a fault holds partitioned stay so.
fn crash_daemon(sim: &mut turmoil::Sim<'_>, partitioned: Links) {
    sim.partition(DAEMON, TUI);
    sim.partition(DAEMON, ANTHROPIC);
    sim.crash(DAEMON);
    if !partitioned.model {
        sim.repair(DAEMON, ANTHROPIC);
    }
    if !partitioned.tui {
        sim.repair(DAEMON, TUI);
    }
}

/// Records each [`Observation`] of the daemon's shell in `ledger`.
///
/// A barrier's condition runs inside the shell's trigger, on the daemon's
/// task, so recording there puts the observation in the ledger's order
/// exactly where it happened. It then declines it: an observation never
/// wakes a barrier, and the shell never waits on one.
fn observer(ledger: &Ledger) -> SimBarrier<Observation> {
    let ledger = ledger.clone();
    SimBarrier::new(move |observation: &Observation| {
        ledger.record(Entry::Shell(observation.clone()));
        false
    })
}

fn failed(seed: u64, sim: &turmoil::Sim<'_>, ledger: &Ledger, reason: String) -> RunError {
    RunError::Failed {
        seed,
        elapsed: sim.elapsed(),
        reason,
        trace: ledger.trace(),
    }
}

/// Runs `scenario` (its seed draws everything it leaves open), with the
/// defect `planted` in the daemon (none, but in the mutation tests), and
/// judges it.
///
/// # Errors
/// A host failed (the daemon returned an error or the TUI stopped early),
/// or the run did not finish within its simulated time limit.
pub fn once(scenario: &Scenario, planted: Planted) -> Result<Outcome, RunError> {
    let seed = scenario.seed;
    let ledger = Ledger::new(LEDGER_CAPACITY);
    let mut builder = turmoil::Builder::new();
    builder
        .rng_seed(seed)
        .enable_random_order()
        .simulation_duration(RUN_LIMIT)
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(scenario.max_latency);
    if let Some(block_size) = scenario.block_size {
        builder.fs().block_size(block_size);
    }
    let mut sim = builder.build();
    let _observer = observer(&ledger);
    sim.host(ANTHROPIC, hosts::anthropic());
    sim.host(
        DAEMON,
        hosts::daemon(ledger.clone(), seed, scenario.limits.clone(), planted),
    );
    sim.client(
        TUI,
        driver::tui(scenario.turns.clone(), seed, ledger.clone()),
    );
    let mut faults = Faults::new(scenario);
    loop {
        let finished = sim
            .step()
            .map_err(|error| failed(seed, &sim, &ledger, error.to_string()))?;
        if finished {
            break;
        }
        faults.tick(&mut sim, &ledger);
    }
    faults.heal(&mut sim, &ledger);
    let recovered = ledger.recoveries();
    crash_daemon(
        &mut sim,
        Links {
            tui: false,
            model: false,
        },
    );
    ledger.record_at(sim.elapsed(), Entry::Fault("final crash".to_owned()));
    sim.bounce(DAEMON);
    let mut steps = 0;
    while ledger.recoveries() == recovered {
        if steps == RECOVERY_STEPS {
            return Err(failed(
                seed,
                &sim,
                &ledger,
                "the daemon did not recover after the final crash".to_owned(),
            ));
        }
        sim.step()
            .map_err(|error| failed(seed, &sim, &ledger, error.to_string()))?;
        steps = steps.saturating_add(1);
    }
    let entries = ledger.entries();
    let violations = check::world(&entries, scenario);
    let observed = model::observed(&entries);
    let replayed = model::replay(scenario.limits.core.clone(), &observed);
    Ok(Outcome {
        scenario: scenario.clone(),
        entries,
        violations,
        observed,
        replayed,
    })
}

/// Runs `scenario` twice, with nothing planted, and compares the traces.
///
/// # Errors
/// Either run failed, or the traces differ.
pub fn twice(scenario: &Scenario) -> Result<Outcome, RunError> {
    let first = once(scenario, Planted::None)?;
    let second = once(scenario, Planted::None)?;
    let (trace, other) = (first.trace(), second.trace());
    if trace == other {
        return Ok(first);
    }
    let line = trace
        .iter()
        .zip(&other)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| trace.len().min(other.len()));
    Err(RunError::Diverged {
        seed: scenario.seed,
        line,
        other: other.get(line).cloned(),
        trace,
    })
}
