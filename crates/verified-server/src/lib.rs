//! The verified daemon.
//!
//! A shell loop with one bounded inbox per session feeds events to the core,
//! persists the records it returns, applies them once the log acknowledges,
//! and runs effects through the seams (model, log, environment, clock,
//! entropy, lock). It serves commands and the Durable Stream over axum and
//! talks to Anthropic over hyper. The `sim` feature swaps the IO shim to
//! turmoil.
//!
//! Entry point: [`daemon::run`] (or [`daemon::serve`] with a custom log
//! store), given a listener, [`shell::Seams`] and a [`shell::ShellConfig`].

pub mod daemon;
pub mod env;
pub mod history;
pub mod limits;
pub mod log;
pub mod model;
pub mod payload;
pub mod projection;
pub mod routes;
pub mod rules_file;
pub mod seam;
pub mod shell;
pub mod shim;
pub mod tool;

/// Crash-only: a broken invariant kills the process; recovery is the way back.
#[expect(
    clippy::panic,
    reason = "crash-only: a broken invariant kills the process and recovery is the way back"
)]
pub(crate) fn defect(what: &str) -> ! {
    panic!("defect: {what}")
}
