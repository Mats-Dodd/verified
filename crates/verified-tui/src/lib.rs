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

pub use tea::run;

use verified_domain::id::SessionId;

use crate::seam::{Clock, Entropy};

/// A fresh session id, minted from the seams.
#[must_use]
pub fn new_session(clock: &dyn Clock, entropy: &mut dyn Entropy) -> SessionId {
    SessionId::from_parts(clock.now(), &entropy.id_entropy())
}
