//! The shell's planted defects that tamper with a decision before it is
//! persisted, under `sim` only. Each has a DST test that fails its check;
//! the one-line plants stay where their bug would be.

use std::collections::BTreeSet;

use verified_core::Effect;
use verified_domain::{EnvelopeId, Exit, Record, Unstarted};

use super::{Planted, Session};
use crate::payload::{Parsed, ToolArgs, ToolOutput, measure};

#[expect(
    clippy::multiple_inherent_impl,
    reason = "the planted defects, under `sim` only, kept apart from the shell's control flow"
)]
impl Session {
    /// `records` and `effects` as the planted defect, if any, makes them.
    /// Runs it starts early are started here, before the records are
    /// durable.
    pub(super) fn planted_decision(
        &mut self,
        records: Vec<Record<Parsed>>,
        mut effects: Vec<Effect<Parsed>>,
    ) -> (Vec<Record<Parsed>>, Vec<Effect<Parsed>>) {
        match self.config.planted {
            Planted::UnknownAsNotStarted => (unknown_as_not_started(records), effects),
            Planted::ConsumeAtInjection => self.consumed_at_injection(records, effects),
            // The decision's tool runs start at once, before its records
            // are durable.
            Planted::RunBeforePersist => {
                for run in effects.extract_if(.., |effect| matches!(effect, Effect::RunTool { .. }))
                {
                    self.start_effect(run);
                }
                (records, effects)
            }
            // Likewise for its deliveries.
            Planted::DeliverBeforePersist => {
                for run in effects.extract_if(.., |effect| {
                    matches!(effect, Effect::RunTool { call }
                        if matches!(call.args(), ToolArgs::Send { .. } | ToolArgs::Spawn { .. }))
                }) {
                    self.start_effect(run);
                }
                (records, effects)
            }
            Planted::None
            | Planted::RunTwice
            | Planted::StoppedAsNotStarted
            | Planted::ActAfterRefusal
            | Planted::NoDeadline
            | Planted::AckBeforeAppend
            | Planted::FreshKeyOnResume
            | Planted::StopSkipsChildren
            | Planted::SpliceSteering
            | Planted::UntoldTaskEnds
            | Planted::SkipFsync => (records, effects),
        }
    }

    /// The planted defect: a message injected into a running turn counts
    /// as consumed then. A decision that starts a turn carrying one (the
    /// turn it steered ended first) is cut before that `TurnStarted`, and
    /// the model call it starts dropped: the message stays unread.
    fn consumed_at_injection(
        &self,
        mut records: Vec<Record<Parsed>>,
        mut effects: Vec<Effect<Parsed>>,
    ) -> (Vec<Record<Parsed>>, Vec<Effect<Parsed>>) {
        // Whether a turn is open, and the envelopes delivered while one was.
        let mut open = false;
        let mut injected = BTreeSet::new();
        let track = |record: &Record<Parsed>,
                     open: &mut bool,
                     injected: &mut BTreeSet<EnvelopeId>| match record {
            Record::TurnStarted { .. } => *open = true,
            Record::TurnEnded { .. } => *open = false,
            Record::Delivered { envelope } if *open => {
                injected.insert(envelope.id);
            }
            Record::Born { .. }
            | Record::RulesLoaded { .. }
            | Record::Delivered { .. }
            | Record::StepStarted { .. }
            | Record::ModelOutput { .. }
            | Record::StepReset { .. }
            | Record::StepFinished { .. }
            | Record::Approved { .. }
            | Record::Dispatched { .. }
            | Record::ToolResult { .. }
            | Record::TaskEnded { .. }
            | Record::StopSent { .. }
            | Record::CancelRequested { .. }
            | Record::EpochBumped { .. } => {}
        };
        for record in &self.records {
            track(record, &mut open, &mut injected);
        }
        let mut cut = None;
        for (at, record) in records.iter().enumerate() {
            if matches!(record, Record::TurnStarted { .. })
                && let Some(Record::StepStarted { carries, .. }) =
                    records.get(at..).and_then(|rest| {
                        rest.iter()
                            .find(|later| matches!(later, Record::StepStarted { .. }))
                    })
                && carries.iter().any(|id| injected.contains(id))
            {
                cut = Some(at);
                break;
            }
            track(record, &mut open, &mut injected);
        }
        if let Some(cut) = cut {
            records.truncate(cut);
            let from = self
                .core
                .seq()
                .saturating_add(u64::try_from(cut).unwrap_or(u64::MAX));
            effects
                .retain(|effect| !matches!(effect, Effect::CallModel { id, .. } if id.seq >= from));
        }
        (records, effects)
    }
}

/// The planted defect: `records` with every `Unknown` result made a
/// `NotStarted` report, as if the shell knew the call never ran. It invents
/// the report (an empty output): the core refuses a synthesized `NotStarted`
/// for a dispatched call (T2), so a shell can only lie past it this way.
fn unknown_as_not_started(mut records: Vec<Record<Parsed>>) -> Vec<Record<Parsed>> {
    for record in &mut records {
        if let Record::ToolResult { exit, output, .. } = record
            && *exit == Exit::Unknown
            && output.is_none()
        {
            *exit = Exit::NotStarted(Unstarted::Cancelled);
            *output = Some(measure(ToolOutput::complete(String::new())));
        }
    }
    records
}
