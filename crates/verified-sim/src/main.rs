//! `verified-sim`: run simulation seeds outside the test harness.
//!
//! ```text
//! verified-sim run --from 0 --count 1000     # nightly (`just sweep`): many seeds, each run twice
//! verified-sim directed NAME                # one directed scenario, twice
//! verified-sim trace --seed 42               # one run of one seed, its trace
//! verified-sim processes                     # each fixed seed in two fresh processes
//! verified-sim coverage --count 200          # model coverage: the gap table
//! ```
//!
//! `processes` catches what running a seed twice in one process cannot:
//! randomness drawn once per process (hash seeds, tokio's internal RNG),
//! which would make a failing seed impossible to replay.

use std::io::{self, Write as _};

use clap::{Parser, Subcommand};
use verified_server::shell::Planted;
use verified_sim::run::{self, Broken, Outcome, RunError};
use verified_sim::scenario::Scenario;
use verified_sim::{coverage, directed};

#[derive(Debug, Parser)]
#[command(version, about = "Deterministic simulation of verified")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Runs `count` seeds from `from`, each twice, and stops at the first
    /// failure.
    Run {
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long)]
        count: u64,
    },
    /// Runs one directed scenario, by name, twice.
    Directed { name: String },
    /// Runs one seed once and prints its trace (diff two of these to find
    /// nondeterminism).
    Trace {
        #[arg(long)]
        seed: u64,
    },
    /// Traces each seed in two fresh processes and compares them: the
    /// fixed seeds, or `count` from `from`.
    Processes {
        #[arg(long, requires = "count")]
        from: Option<u64>,
        #[arg(long, requires = "from")]
        count: Option<u64>,
    },
    /// Prints the coverage keys BFS reaches under the model checks' bounds
    /// against those `count` seeds from `from` reach, and each gap
    /// classified.
    Coverage {
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long)]
        count: u64,
    },
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Broken(#[from] Broken),
    #[error("writing the report: {0}")]
    Io(#[from] io::Error),
    #[error("tracing seed {seed} in a fresh process failed")]
    Child { seed: u64 },
    #[error(transparent)]
    Coverage(#[from] coverage::CoverageError),
    #[error(transparent)]
    Directed(#[from] directed::DirectedError),
    #[error("no directed scenario is named {0}")]
    NoDirected(String),
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

fn report(out: &mut impl io::Write, outcome: Outcome) -> Result<(), Failure> {
    writeln!(
        out,
        "seed {}: {} trace lines, {} violations",
        outcome.scenario.seed,
        outcome.entries.len(),
        outcome.violations.len()
    )?;
    match &outcome.replayed {
        Ok(replayed) => writeln!(
            out,
            "  replayed {} observations through the model, reaching {} coverage keys",
            replayed.observations,
            replayed.keys.len()
        )?,
        Err(mismatch) => writeln!(out, "  {mismatch}")?,
    }
    for violation in &outcome.violations {
        writeln!(out, "  {violation}")?;
    }
    outcome.verdict()?;
    Ok(())
}

fn main() -> Result<(), Failure> {
    let args = Args::parse();
    let mut out = io::stdout().lock();
    match args.command {
        Command::Directed { name } => {
            let chosen = directed::named(&name).ok_or(Failure::NoDirected(name))?;
            writeln!(out, "target: {}\n{:#?}", chosen.target, chosen.scenario)?;
            for key in directed::run(&chosen)?.keys {
                writeln!(out, "  reached {key}")?;
            }
            Ok(())
        }
        Command::Run { from, count } => {
            for seed in from..from.saturating_add(count) {
                report(&mut out, run::twice(&Scenario::from_seed(seed))?)?;
            }
            Ok(())
        }
        Command::Trace { seed } => {
            let trace = match run::once(&Scenario::from_seed(seed), Planted::None) {
                Ok(outcome) => outcome.trace(),
                Err(error) => error.trace().to_vec(),
            };
            for line in &trace {
                writeln!(out, "{line}")?;
            }
            Ok(())
        }
        Command::Processes { from, count } => {
            let seeds: Vec<u64> = match from.zip(count) {
                Some((from, count)) => (from..from.saturating_add(count)).collect(),
                None => run::FIXED.to_vec(),
            };
            for seed in seeds {
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
    }
}
