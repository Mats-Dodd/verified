//! The types every verified crate shares, defined once: ids, durable
//! records and their parts, the `Limits` value, and the payload slots.
//!
//! Types and pure helpers only. Everything the core's proofs mention is
//! written inside `verus!` and checked by `just verify` along with the core.
#![cfg_attr(
    verus_keep_ghost,
    expect(
        unstable_features,
        reason = "Verus injects the nightly features it needs while verifying; plain builds stay on stable"
    )
)]

pub mod compaction;
pub mod envelope;
pub mod id;
pub mod limits;
pub mod payload;
pub mod record;
pub mod rules;
pub mod shape;

pub use compaction::{INSTRUCTION, OPEN_ENTRY_BYTES};
pub use envelope::{Address, Body, Envelope, EnvelopeId};
pub use id::{
    ActorId, ApprovalId, CallId, CallIx, EffectId, Epoch, IdEntropy, IdError, MessageId, PartId,
    StepId, TaskId, ToolName, TurnId, UnixMillis,
};
pub use limits::{Limits, LimitsConfig, Millis};
pub use payload::{Abstract, Codec, Measured, Payload, Unit};
pub use record::{
    Class, Exit, Failure, Limit, ModelFault, OpenWork, Proposal, Record, Request, Reset, Tool,
    TurnOutcome, Unstarted, named,
};
pub use rules::{ActorName, Born, Grant, NameError, Pattern, Rule, Rules};
pub use shape::Shape;
