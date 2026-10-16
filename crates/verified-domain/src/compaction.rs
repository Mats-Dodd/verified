//! Model history compaction: the built-in instruction a summarize call
//! ends with, and what one open entry beside the summary may cost.
//!
//! Compaction never cuts the log. The core decides that a compaction
//! happens and where its cut is (`CompactionStarted`); the shell makes the
//! summarize call and the core records its summary (`Compacted`). From then
//! on the model is sent the summary, the actor's open work, and what
//! follows the cut.

use vstd::prelude::*;

verus! {

/// The most one entry of a `Compacted`'s open work adds to the
/// conversation.
///
/// An entry (a running task, a child) is one line beside the summary. The
/// core counts each at this size when a compaction resets the
/// conversation's size; the renderer keeps every line within it.
pub const OPEN_ENTRY_BYTES: u64 = 256;

} // verus!

/// What the summarize call asks of the model, after the conversation it
/// summarizes. Built in, not configurable.
pub const INSTRUCTION: &str = "Summarize the conversation so far for your own later use: it will \
     replace everything above it. Keep what you were asked to do, what you decided and why, what \
     you learned, the files and commands that matter, and what is left to do. Write plain text \
     and call no tools.";
