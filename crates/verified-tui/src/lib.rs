//! The verified terminal client.
//!
//! An Elm loop where `update` returns commands as data ([`tea`]), a view over
//! the folded transcript ([`view`]), and a client that posts commands with
//! client-minted ids and follows the daemon's Durable Stream, reconnecting
//! from its last offset ([`client`]). Nondeterminism enters only through
//! [`seam`]; the `sim` feature swaps the network in [`shim`] to turmoil.
//!
//! [`run`] is the entry point for the binary and for simulation alike.

pub mod app;
pub mod client;
pub mod limits;
pub mod seam;
pub mod shim;
pub mod tea;
pub mod terminal;
pub mod view;

pub use limits::{Limits, Schedule};

use verified_domain::id::SessionId;

use crate::app::{App, Flags, Start};
use crate::seam::{Clock, Entropy, Screen};
use crate::tea::{RunError, Seams};

/// A fresh session id, minted from the seams.
#[must_use]
pub fn new_session(clock: &dyn Clock, entropy: &mut dyn Entropy) -> SessionId {
    SessionId::from_parts(clock.now(), &entropy.id_entropy())
}

/// Runs the TUI on `session` until the user quits or input ends, and returns
/// the final app (its transcript is the fold of what the stream delivered).
///
/// `observe` sees the app after every redraw and after every update.
///
/// # Errors
/// The terminal failed.
///
/// # Panics
/// An effect task panicked (crash-only).
pub async fn run(
    start: Start,
    limits: Limits,
    seams: Seams,
    screen: &mut dyn Screen,
    observe: &mut dyn FnMut(&App),
) -> Result<App, RunError> {
    tea::run::<App>(Flags { start, limits }, limits, seams, screen, observe).await
}
