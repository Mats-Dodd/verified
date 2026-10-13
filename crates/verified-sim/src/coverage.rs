//! Model coverage of the simulation.
//!
//! The coverage keys ([`verified_model::coverage`]) a range of seeds
//! reaches, replayed through the model, against those BFS reaches under the
//! bounds the model checks run with ([`verified_model::suite`]). Every key
//! BFS reaches and no seed
//! did is a gap, classified as an over-approximation, a fidelity gap or a
//! test gap.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use verified_model::coverage::{Gap, Key, classify, explore};
use verified_model::{Action, suite};

use crate::directed::{self, DirectedError};
use crate::run::{self, RunError};

/// What BFS reached under every bound: each key, the bounds that reach
/// it, and the shortest path found to it.
#[derive(Debug, Clone, Default)]
pub struct Modelled {
    pub states: usize,
    pub keys: BTreeMap<Key, (Vec<&'static str>, Vec<Action>)>,
}

/// BFS over every bound the model checks run with.
#[must_use]
pub fn modelled() -> Modelled {
    let mut modelled = Modelled::default();
    for (name, harness) in suite::all() {
        let explored = explore(&harness);
        modelled.states = modelled.states.saturating_add(explored.states);
        for (key, witness) in explored.witnesses {
            let (bounds, shortest) = modelled
                .keys
                .entry(key)
                .or_insert_with(|| (Vec::new(), witness.clone()));
            bounds.push(name);
            if witness.len() < shortest.len() {
                *shortest = witness;
            }
        }
    }
    modelled
}

/// Why a seed's coverage could not be read.
#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Directed(#[from] DirectedError),
    #[error("seed {seed} broke invariants, so its coverage means nothing: {violations}")]
    Violated { seed: u64, violations: String },
}

/// What the simulation reached: each key with the seeds that reach it,
/// and with the directed scenarios ([`crate::directed`]) that do.
#[derive(Debug, Clone, Default)]
pub struct Simulated {
    pub seeds: BTreeMap<Key, Vec<u64>>,
    pub directed: BTreeMap<Key, Vec<&'static str>>,
}

/// The keys of `outcome`'s replay.
fn reached(seed: u64, outcome: run::Outcome) -> Result<BTreeSet<Key>, CoverageError> {
    match outcome.replayed {
        Some(replayed) if outcome.violations.is_empty() => Ok(replayed.keys),
        Some(_) | None => Err(CoverageError::Violated {
            seed,
            violations: format!("{:?}", outcome.violations),
        }),
    }
}

/// Each key the seeds `from..from + count` reach, and each every directed
/// scenario reaches.
///
/// # Errors
/// A run failed or broke an invariant (the model's replay among them), or
/// a directed scenario failed ([`directed::run`]).
pub fn simulated(from: u64, count: u64) -> Result<Simulated, CoverageError> {
    let mut simulated = Simulated::default();
    for seed in from..from.saturating_add(count) {
        for key in reached(seed, run::simulate(seed)?)? {
            simulated.seeds.entry(key).or_default().push(seed);
        }
    }
    for chosen in directed::all() {
        for key in directed::run(&chosen)?.keys {
            simulated.directed.entry(key).or_default().push(chosen.name);
        }
    }
    Ok(simulated)
}

/// The gaps: each key BFS reaches and no seed did, with its class.
#[must_use]
pub fn gaps(modelled: &Modelled, simulated: &Simulated) -> Vec<(Key, Gap, &'static str)> {
    modelled
        .keys
        .keys()
        .filter(|key| !simulated.seeds.contains_key(key))
        .map(|key| {
            let (gap, why) = classify(key);
            (*key, gap, why)
        })
        .collect()
}

/// `witness` as its actions' kinds.
fn kinds(witness: &[Action]) -> String {
    witness
        .iter()
        .map(|action| format!("{:?}", action.kind()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes the gap table for seeds `from..from + count`.
///
/// # Errors
/// Writing failed.
pub fn table<W: io::Write>(
    out: &mut W,
    (from, count): (u64, u64),
    modelled: &Modelled,
    simulated: &Simulated,
) -> io::Result<()> {
    let gaps = gaps(modelled, simulated);
    let both = modelled
        .keys
        .keys()
        .filter(|key| simulated.seeds.contains_key(key))
        .count();
    writeln!(
        out,
        "coverage: seeds {from}..{} and {} directed scenarios, against BFS under {} \
         bounds ({} states)",
        from.saturating_add(count),
        directed::all().len(),
        suite::all().len(),
        modelled.states
    )?;
    writeln!(
        out,
        "{} keys in BFS, {} reached by seeds, {both} in both\n",
        modelled.keys.len(),
        simulated.seeds.len()
    )?;
    writeln!(out, "reached by both: key, seeds reaching it, the first")?;
    for (key, seeds) in &simulated.seeds {
        if modelled.keys.contains_key(key) {
            writeln!(
                out,
                "  {:<40} {:>5}  seed {}",
                key.to_string(),
                seeds.len(),
                seeds.first().copied().unwrap_or_default()
            )?;
        }
    }
    writeln!(out, "\ngaps: in BFS, in no seed")?;
    for (key, gap, why) in &gaps {
        let witness = modelled
            .keys
            .get(key)
            .map(|(bounds, witness)| (bounds.join(", "), kinds(witness)))
            .unwrap_or_default();
        let closed = simulated
            .directed
            .get(key)
            .map_or_else(|| "none".to_owned(), |names| names.join(", "));
        writeln!(out, "  {:<40} {gap:?}", key.to_string())?;
        writeln!(out, "      why: {why}")?;
        writeln!(out, "      directed: {closed}")?;
        writeln!(out, "      bounds: {}", witness.0)?;
        writeln!(out, "      witness: {}", witness.1)?;
    }
    writeln!(
        out,
        "\nbeyond the bounds: reached by seeds, in no BFS (the bounds are smaller than v0's)"
    )?;
    for key in simulated.seeds.keys() {
        if !modelled.keys.contains_key(key) {
            writeln!(out, "  {key}")?;
        }
    }
    let count_of = |class: Gap| gaps.iter().filter(|(_, gap, _)| *gap == class).count();
    let open = gaps
        .iter()
        .filter(|(key, gap, _)| *gap == Gap::Test && !simulated.directed.contains_key(key))
        .count();
    writeln!(
        out,
        "\n{} gaps: {} over-approximation, {} fidelity, {} test ({open} with no directed \
         scenario)",
        gaps.len(),
        count_of(Gap::OverApproximation),
        count_of(Gap::Fidelity),
        count_of(Gap::Test)
    )
}
