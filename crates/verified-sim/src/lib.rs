//! Deterministic simulation of verified on turmoil.
//!
//! The harness runs the real daemon and TUI (built with their `sim` feature)
//! as turmoil hosts next to a fake Anthropic host, with a fake shell whose
//! side effects land in a world [`ledger`] that survives crashes. A seed
//! draws a [`scenario`]: what the user types and answers, crashes (at random
//! times and aimed at barriers inside the daemon), partitions between the
//! TUI and the daemon, and torn writes. After each run [`check`] judges the
//! world (I1-I8), and [`run::twice`] runs each seed twice to prove the trace
//! is deterministic.

pub mod check;
pub mod driver;
pub mod entropy;
pub mod env;
pub mod hosts;
pub mod ledger;
pub mod run;
pub mod scenario;
pub mod store;
pub mod trace;
