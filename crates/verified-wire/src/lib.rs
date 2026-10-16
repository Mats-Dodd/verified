//! Wire types shared by the daemon and the TUI.
//!
//! Bytes become domain types here exactly once: client commands with
//! client-minted ids (the id types themselves live in `verified-domain`), AI SDK UI chunks,
//! Durable Streams control, the transcript fold and the SSE codec. No async
//! and no IO. Every decoder takes a [`WireLimits`] so nothing grows without
//! a bound.

pub mod chunk;
pub mod command;
pub mod limits;
pub mod partial_json;
pub mod sse;
pub mod stream;
pub mod transcript;

pub use limits::WireLimits;
