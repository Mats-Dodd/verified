//! A Stateright model of the core inside an abstract world.
//!
//! The world is the durable log, the decision being persisted (which a crash
//! may tear to any prefix), effects in flight with an Execute/Complete split
//! for tools, effects orphaned by a crash (which may still execute and
//! reply), and facts about the outside: how often each call ran and which
//! approvals the user was asked for. The core is always `recover(log)`: it
//! has no other state, and it runs on [`Abstract`](verified_domain::Abstract)
//! payloads, which keep every id and size ([`alpha`]). A client follows the
//! stream: it reads committed records from its offset, may reconnect, and
//! must only ever see a prefix of the durable log (the server projects
//! records to chunks one record per offset, so the projection is modelled as
//! record-seq coverage).
//!
//! The shell delivers one input to the core at a time and persists each
//! decision before taking the next, but the world does not wait for it:
//! tool runs (and orphans of a crash) act, and clients read, while a
//! decision is being persisted. A model call streams any number of batches.
//!
//! Two uses share one [`Harness`]: BFS over a small [`Bounded`] domain
//! (`tests/model.rs`), and [`replay()`] of a run's observations, whose actions
//! carry the ids and sizes the run had ([`Domain::Trace`]) under the limits it ran
//! with, checking every property on every state the run passes through.

pub mod alpha;
mod audit;
pub mod coverage;
mod harness;
pub mod replay;
pub mod suite;
mod world;

pub use harness::{
    Action, Bounded, Bug, Domain, Harness, Kind, background, born, message, plain, stop,
};
pub use replay::{Divergence, Mismatch, Observed, Replayed, replay, replay_actors};
pub use world::{Actor, Client, Delivery, Flight, Job, Landing, Rec, Slot, Staged, World};
