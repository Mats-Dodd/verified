//! `verified-sim`: run simulation seeds outside the test harness.
//!
//! ```text
//! verified-sim sweep --from 0 --count 1000   # nightly (`just sweep`): many seeds, each run twice
//! verified-sim replay --seed 42 --trace      # one seed, with its full trace
//! verified-sim trace --seed 42               # one run of one seed, its trace only
//! verified-sim processes --from 0 --count 8  # each seed in two fresh processes
//! verified-sim coverage --from 0 --count 200 # model coverage: the gap table
//! verified-sim directed --name NAME --trace  # one directed scenario, twice
//! ```
//!
//! `processes` catches what running a seed twice in one process cannot:
//! randomness drawn once per process (hash seeds, tokio's internal RNG),
//! which would make a failing seed impossible to replay.

use std::io::{self, Write as _};

use clap::{Parser, Subcommand};
use verified_sim::run::{self, Outcome, RunError};
use verified_sim::{coverage, directed};

#[derive(Debug, Parser)]
#[command(version, about = "Deterministic simulation of verified")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Runs `count` seeds from `from`, each twice, and stops at the first failure.
    Sweep {
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = 100)]
        count: u64,
    },
    /// Runs one seed twice and reports it.
    Replay {
        #[arg(long)]
        seed: u64,
        /// Prints the whole trace.
        #[arg(long)]
        trace: bool,
    },
    /// Traces each seed in two fresh processes and compares them.
    Processes {
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = 8)]
        count: u64,
    },
    /// Prints the coverage keys BFS reaches under the model checks' bounds
    /// against those the seeds reach, and each gap classified.
    Coverage {
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long, default_value_t = 200)]
        count: u64,
    },
    /// Compiles one directed scenario from its BFS witness, runs it twice
    /// and reports it, with the coverage keys its replay reached.
    Directed {
        #[arg(long)]
        name: String,
        /// Prints the whole trace.
        #[arg(long)]
        trace: bool,
    },
    /// Runs one seed once and prints its trace (diff two of these to find
    /// nondeterminism).
    Trace {
        #[arg(long)]
        seed: u64,
        /// Runs it this many times in this process, one trace after another.
        #[arg(long, default_value_t = 1)]
        runs: u32,
    },
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("seed {seed} broke {count} invariants")]
    Violated { seed: u64, count: usize },
    #[error("writing the report: {0}")]
    Io(#[from] io::Error),
    #[error("tracing seed {seed} in a fresh process failed")]
    Child { seed: u64 },
    #[error(transparent)]
    Coverage(#[from] coverage::CoverageError),
    #[error(transparent)]
    Compile(#[from] directed::CompileError),
    #[error("no directed scenario is named {0}")]
    NoDirected(String),
    #[error("directed scenario {0} missed its target")]
    Missed(String),
    #[error("seed {seed} traced differently in two processes, from line {line}")]
    Diverged { seed: u64, line: usize },
}

/// The trace of `seed` from a fresh process running this binary.
#[expect(
    clippy::disallowed_methods,
    reason = "the harness's own boundary: it re-runs itself in a fresh process to catch per-process randomness"
)]
fn trace_in_process(seed: u64) -> Result<Vec<u8>, Failure> {
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["trace", "--seed", &seed.to_string()])
        .output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Failure::Child { seed })
    }
}

fn report(out: &mut impl io::Write, outcome: &Outcome) -> Result<(), Failure> {
    writeln!(
        out,
        "seed {}: {} trace lines, {} violations",
        outcome.scenario.seed,
        outcome.trace.len(),
        outcome.violations.len()
    )?;
    if let Some(replayed) = &outcome.replayed {
        writeln!(
            out,
            "  replayed {} observations through the model (frontier at most {}): {:?}",
            replayed.observations, replayed.max_frontier, replayed.actions
        )?;
    }
    for violation in &outcome.violations {
        writeln!(out, "  {violation}")?;
    }
    if outcome.violations.is_empty() {
        Ok(())
    } else {
        Err(Failure::Violated {
            seed: outcome.scenario.seed,
            count: outcome.violations.len(),
        })
    }
}

fn main() -> Result<(), Failure> {
    let args = Args::parse();
    let mut out = io::stdout().lock();
    match args.command {
        Command::Sweep { from, count } => {
            for seed in from..from.saturating_add(count) {
                let outcome = run::twice(seed)?;
                report(&mut out, &outcome)?;
            }
            Ok(())
        }
        Command::Replay { seed, trace } => {
            let outcome = match run::twice(seed) {
                Ok(outcome) => outcome,
                Err(error) => {
                    for line in &error.trace {
                        writeln!(out, "{line}")?;
                    }
                    return Err(error.into());
                }
            };
            writeln!(out, "{:#?}", outcome.scenario)?;
            if trace {
                for line in &outcome.trace {
                    writeln!(out, "{line}")?;
                }
            }
            report(&mut out, &outcome)
        }
        Command::Processes { from, count } => {
            for seed in from..from.saturating_add(count) {
                let first = trace_in_process(seed)?;
                let second = trace_in_process(seed)?;
                if first != second {
                    let line = first
                        .split(|byte| *byte == b'\n')
                        .zip(second.split(|byte| *byte == b'\n'))
                        .position(|(left, right)| left != right)
                        .unwrap_or_default();
                    return Err(Failure::Diverged { seed, line });
                }
                writeln!(out, "seed {seed}: the same in two processes")?;
            }
            Ok(())
        }
        Command::Coverage { from, count } => {
            let simulated = coverage::simulated(from, count)?;
            let modelled = coverage::modelled();
            coverage::table(&mut out, (from, count), &modelled, &simulated)?;
            Ok(())
        }
        Command::Directed { name, trace } => {
            let Some(chosen) = directed::all()
                .into_iter()
                .find(|chosen| chosen.name == name)
            else {
                return Err(Failure::NoDirected(name));
            };
            let witness = directed::witness(&chosen)?;
            writeln!(out, "target: {}\nwitness: {witness:?}", chosen.target)?;
            let scenario = directed::compile(&chosen.harness, &witness, chosen.seed)?;
            let outcome = run::twice_scenario(&scenario)?;
            writeln!(out, "{:#?}", outcome.scenario)?;
            if trace {
                for line in &outcome.trace {
                    writeln!(out, "{line}")?;
                }
            }
            let reached = outcome.replayed.as_ref().map(|replayed| &replayed.keys);
            if let Some(keys) = reached {
                for key in keys {
                    writeln!(out, "  reached {key}")?;
                }
            }
            report(&mut out, &outcome)?;
            if reached.is_some_and(|keys| keys.contains(&chosen.target)) {
                Ok(())
            } else {
                Err(Failure::Missed(name))
            }
        }
        Command::Trace { seed, runs } => {
            for run in 0..runs {
                let trace = match run::simulate(seed) {
                    Ok(outcome) => outcome.trace,
                    Err(error) => error.trace,
                };
                writeln!(out, "run {run}")?;
                for line in &trace {
                    writeln!(out, "{line}")?;
                }
            }
            Ok(())
        }
    }
}
