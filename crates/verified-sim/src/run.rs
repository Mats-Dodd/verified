//! One simulated run: build the world from a seed, inject its faults, let the
//! scripted user finish, crash the daemon one last time, and judge the world.

use core::time::Duration;
use std::collections::VecDeque;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use turmoil::barriers::{Barrier as SimBarrier, Reaction, Triggered};
use verified_server::shell::Planted;
use verified_server::shim::Barrier;

use crate::check::{self, Violation};
use crate::driver;
use crate::env::Point;
use crate::hosts::{self, ANTHROPIC, DAEMON, TUI};
use crate::ledger::{Entry, Ledger};
use crate::scenario::{Aim, AimedCrash, Fault, Scenario};

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
    /// The ledger as text, one line per entry: the determinism trace.
    pub trace: Vec<String>,
    pub violations: Vec<Violation>,
}

/// Why a run did not finish.
#[derive(Debug, thiserror::Error)]
#[error("seed {seed}: the simulation failed after {elapsed:?}: {reason}")]
pub struct RunError {
    pub seed: u64,
    pub elapsed: Duration,
    pub reason: String,
    /// The ledger up to the failure.
    pub trace: Vec<String>,
}

/// What the harness must undo later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Undo {
    Bounce,
    Repair,
    RepairModel,
}

/// The barrier a crash is aimed at: one of the daemon's or one of the
/// simulated environment's.
enum Waiting {
    Daemon(SimBarrier<Barrier>),
    Env(SimBarrier<Point>),
}

/// A barrier reached, holding its task suspended until dropped.
enum Hit {
    Daemon(Triggered<Barrier>),
    Env(Triggered<Point>),
}

impl Waiting {
    fn new(aim: Aim) -> Self {
        match aim {
            Aim::Dispatched | Aim::ToolRan | Aim::GroupWritten => Self::Daemon(SimBarrier::build(
                Reaction::Suspend,
                move |point: &Barrier| aimed_at(aim, point),
            )),
            Aim::EditMarked => Self::Env(SimBarrier::build(Reaction::Suspend, |point: &Point| {
                matches!(point, Point::EditMarked)
            })),
        }
    }

    /// The barrier reached since the last poll, if any.
    fn poll(&mut self) -> Option<Hit> {
        match self {
            Self::Daemon(barrier) => poll_once(barrier.wait()).flatten().map(Hit::Daemon),
            Self::Env(barrier) => poll_once(barrier.wait()).flatten().map(Hit::Env),
        }
    }
}

impl Hit {
    fn describe(&self) -> String {
        match self {
            Self::Daemon(point) => format!("{:?}", **point),
            Self::Env(point) => format!("{:?}", **point),
        }
    }

    fn release(self) {
        match self {
            Self::Daemon(triggered) => drop(triggered),
            Self::Env(triggered) => drop(triggered),
        }
    }
}

/// The crash aimed at a barrier, while it waits to fire.
struct Aimed {
    plan: AimedCrash,
    barrier: Waiting,
    passed: u32,
}

/// The faults of a run, applied as simulated time passes.
struct Faults {
    due: VecDeque<(Duration, Fault)>,
    undo: Vec<(Duration, Undo)>,
    aimed: Option<Aimed>,
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
        (Aim::Dispatched, Barrier::Dispatched { .. })
            | (Aim::ToolRan, Barrier::ToolRan { .. })
            | (Aim::GroupWritten, Barrier::GroupWritten { .. })
    )
}

impl Faults {
    fn new(scenario: &Scenario) -> Self {
        let aimed = scenario.aimed.map(|plan| Aimed {
            plan,
            barrier: Waiting::new(plan.aim),
            passed: 0,
        });
        Self {
            due: scenario.faults.iter().copied().collect(),
            undo: Vec::new(),
            aimed,
            daemon_up: true,
            partitioned: 0,
            model_partitioned: 0,
        }
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
        self.undo.push((now.saturating_add(down), Undo::Bounce));
    }

    fn undo(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger, undo: Undo) {
        let now = sim.elapsed();
        match undo {
            Undo::Bounce => {
                if !self.daemon_up {
                    sim.bounce(DAEMON);
                    self.daemon_up = true;
                    ledger.record_at(now, Entry::Fault("daemon restarts".to_owned()));
                }
            }
            Undo::Repair => {
                self.partitioned = self.partitioned.saturating_sub(1);
                if self.partitioned == 0 {
                    sim.repair(TUI, DAEMON);
                    ledger.record_at(now, Entry::Fault("the TUI link heals".to_owned()));
                }
            }
            Undo::RepairModel => {
                self.model_partitioned = self.model_partitioned.saturating_sub(1);
                if self.model_partitioned == 0 {
                    sim.repair(DAEMON, ANTHROPIC);
                    ledger.record_at(now, Entry::Fault("the model link heals".to_owned()));
                }
            }
        }
    }

    /// Applies what is due at the simulation's current time.
    fn tick(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger) {
        let now = sim.elapsed();
        while let Some((at, fault)) = self.due.front().copied() {
            if at > now {
                break;
            }
            self.due.pop_front();
            match fault {
                Fault::Crash { down } => {
                    if self.daemon_up {
                        self.crash(sim, ledger, format!("daemon crashes for {down:?}"), down);
                    }
                }
                Fault::Partition { lasting } => {
                    if self.partitioned == 0 {
                        sim.partition(TUI, DAEMON);
                    }
                    self.partitioned = self.partitioned.saturating_add(1);
                    ledger.record_at(
                        now,
                        Entry::Fault(format!("the TUI link partitions for {lasting:?}")),
                    );
                    self.undo.push((now.saturating_add(lasting), Undo::Repair));
                }
                Fault::ModelPartition { lasting } => {
                    if self.model_partitioned == 0 {
                        sim.partition(DAEMON, ANTHROPIC);
                    }
                    self.model_partitioned = self.model_partitioned.saturating_add(1);
                    ledger.record_at(
                        now,
                        Entry::Fault(format!("the model link partitions for {lasting:?}")),
                    );
                    self.undo
                        .push((now.saturating_add(lasting), Undo::RepairModel));
                }
            }
        }
        let mut index = 0;
        while let Some((at, undo)) = self.undo.get(index).copied() {
            if at <= now {
                self.undo.remove(index);
                self.undo(sim, ledger, undo);
            } else {
                index = index.saturating_add(1);
            }
        }
        self.aim(sim, ledger);
    }

    /// Releases each barrier hit until the aimed one, then crashes there.
    fn aim(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger) {
        loop {
            let Some(aimed) = self.aimed.as_mut() else {
                return;
            };
            let Some(hit) = aimed.barrier.poll() else {
                return;
            };
            if aimed.passed < aimed.plan.occurrence || !self.daemon_up {
                aimed.passed = aimed.passed.saturating_add(1);
                hit.release();
                continue;
            }
            let plan = aimed.plan;
            // Crash while suspended at the barrier, then forget the barrier
            // so it does not catch the restarted daemon.
            self.crash(
                sim,
                ledger,
                format!("daemon crashes at {} for {:?}", hit.describe(), plan.down),
                plan.down,
            );
            hit.release();
            self.aimed = None;
        }
    }

    /// Undoes everything still in force.
    fn heal(&mut self, sim: &mut turmoil::Sim<'_>, ledger: &Ledger) {
        self.aimed = None;
        self.due.clear();
        for (_, undo) in core::mem::take(&mut self.undo) {
            self.undo(sim, ledger, undo);
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

fn failed(seed: u64, sim: &turmoil::Sim<'_>, ledger: &Ledger, reason: String) -> RunError {
    RunError {
        seed,
        elapsed: sim.elapsed(),
        reason,
        trace: ledger.trace(),
    }
}

/// Runs the world of `seed` and judges it.
///
/// # Errors
/// A host failed (the daemon returned an error or the TUI stopped early),
/// or the run did not finish within its simulated time limit.
pub fn simulate(seed: u64) -> Result<Outcome, RunError> {
    simulate_with(seed, Planted::default())
}

/// [`simulate`], with defects `planted` in the daemon: the mutation tests
/// show the world checks catch them.
///
/// # Errors
/// As [`simulate`].
pub fn simulate_with(seed: u64, planted: Planted) -> Result<Outcome, RunError> {
    let scenario = Scenario::from_seed(seed);
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
    sim.host(ANTHROPIC, hosts::anthropic());
    sim.host(DAEMON, hosts::daemon(ledger.clone(), seed, planted));
    sim.client(
        TUI,
        driver::tui(scenario.turns.clone(), seed, ledger.clone()),
    );
    let mut faults = Faults::new(&scenario);
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
    let violations = check::world(&ledger.entries(), &scenario);
    Ok(Outcome {
        scenario,
        trace: ledger.trace(),
        violations,
    })
}

/// Runs `seed` twice and compares the traces.
///
/// # Errors
/// Either run failed, or the traces differ (the `reason` names the first
/// differing line).
pub fn twice(seed: u64) -> Result<Outcome, RunError> {
    let first = simulate(seed)?;
    let second = simulate(seed)?;
    if first.trace != second.trace {
        let at = first
            .trace
            .iter()
            .zip(&second.trace)
            .position(|(left, right)| left != right)
            .unwrap_or_else(|| first.trace.len().min(second.trace.len()));
        return Err(RunError {
            seed,
            elapsed: Duration::ZERO,
            reason: format!(
                "two runs diverged at trace line {at}:\n  first:  {}\n  second: {}",
                first.trace.get(at).map_or("<end>", String::as_str),
                second.trace.get(at).map_or("<end>", String::as_str),
            ),
            trace: first.trace,
        });
    }
    Ok(first)
}
