//! Envelopes: what lands in an actor's mailbox.
//!
//! An envelope is delivered by appending a `Delivered` record to its
//! recipient's log; the recipient deduplicates by [`EnvelopeId`], so a
//! sender may deliver one again after a crash and it lands once. Only actors
//! have mailboxes: `user` and `v` are addresses an envelope comes from (or
//! names as its `reply_to`), never ones it is delivered to.

use core::fmt;
use core::str::FromStr;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};
use vstd::prelude::*;

use crate::id::{ActorId, IdError, MessageId, TaskId};
use crate::payload::{Measured, Payload};

verus! {

/// Who an envelope is from, or whom its `reply_to` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub enum Address {
    /// The user.
    User,
    /// The runtime itself, `v`: notifications and stops come from it.
    Runtime,
    /// An actor.
    Actor(ActorId),
}

/// The id a recipient deduplicates on: unique within its log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(rename_all = "camelCase")]
pub enum EnvelopeId {
    /// A user command: the id the client minted (a message or a stop).
    User(MessageId),
    /// Minted from a log: `seq` is the record in `actor`'s log that licensed
    /// the delivery (the `Dispatched` of a `send` or `spawn`, a
    /// `StopSent`). It survives a crash unchanged, so a `Keyed` rerun
    /// delivers the same id again.
    Minted { actor: ActorId, seq: u64 },
    /// The notification that task `TaskId` ended: its `TaskEnded` record,
    /// in the actor's own log. Never delivered from elsewhere.
    Task(TaskId),
}

impl EnvelopeId {
    /// Whether two ids are the same.
    #[must_use]
    pub const fn same(self, other: EnvelopeId) -> (same: bool)
        ensures
            same == (self == other),
    {
        match (self, other) {
            (EnvelopeId::User(left), EnvelopeId::User(right)) => left.same(right),
            (
                EnvelopeId::Minted { actor: left, seq: left_seq },
                EnvelopeId::Minted { actor: right, seq: right_seq },
            ) => left.same(right) && left_seq == right_seq,
            (EnvelopeId::Task(left), EnvelopeId::Task(right)) => left.same(right),
            _ => false,
        }
    }
}

/// What an envelope says.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(rename_all = "camelCase", bound = "P: crate::payload::Codec")]
pub enum Body<P: Payload> {
    /// Text for the recipient's model.
    Text(Measured<P::Text>),
    /// Stop: the recipient revokes everything it holds and stops every
    /// actor it spawned.
    Stop,
}

/// One delivery into an actor's mailbox.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
#[serde(rename_all = "camelCase", bound = "P: crate::payload::Codec")]
pub struct Envelope<P: Payload> {
    pub id: EnvelopeId,
    pub from: Address,
    pub to: ActorId,
    /// Metadata for the model; no one is obliged to answer.
    pub reply_to: Option<Address>,
    pub body: Body<P>,
}

} // verus!

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User => formatter.write_str("user"),
            Self::Runtime => formatter.write_str("v"),
            Self::Actor(actor) => actor.fmt(formatter),
        }
    }
}

impl FromStr for Address {
    type Err = IdError;

    /// `user`, `v`, or an actor's id.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "user" => Ok(Self::User),
            "v" => Ok(Self::Runtime),
            _ => ActorId::parse(text).map(Self::Actor),
        }
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::Address;
    use crate::id::{ActorId, UnixMillis};

    #[test]
    fn addresses_are_text() {
        let actor = ActorId::from_parts(UnixMillis::new(1).expect("in range"), &[3; 10]);
        for address in [Address::User, Address::Runtime, Address::Actor(actor)] {
            let json = serde_json::to_string(&address).expect("addresses serialize");
            assert_eq!(serde_json::from_str::<Address>(&json).ok(), Some(address));
        }
        assert_eq!("user".parse::<Address>(), Ok(Address::User));
        assert_eq!("someone".parse::<Address>().ok(), None, "not an address");
    }
}
