//! The pure core of verified: a deterministic state machine with no clock,
//! randomness or IO.
//!
//! `Core::decide` maps an `Event` to a `Decision` (records to persist, then
//! effects to start); `Core::apply` is the only way durable state changes
//! and runs only after the log acknowledges the record. `decide` plans by
//! applying each record it emits to a scratch copy of the core, so what it
//! decides is apply's semantics by construction. `Core::recover` folds
//! `apply` over a log and `Core::resume` picks up after a crash. Everything
//! here is written in the Verus subset and checked by `just verify`; the
//! types it shares with the other crates live in `verified-domain`.
#![cfg_attr(
    verus_keep_ghost,
    expect(
        unstable_features,
        reason = "Verus injects the nightly features it needs while verifying; plain builds stay on stable"
    )
)]

pub mod effect;
pub mod event;
pub mod machine;
pub mod rules;

pub use effect::{Approved, Decision, Effect, Outcome, Pending, Rejection, ToolCall};
pub use event::{Event, Loaded, ModelExit, Verdict};
pub use machine::{Core, Corrupt, Violation};
