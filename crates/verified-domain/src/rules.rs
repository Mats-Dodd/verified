//! Grants: every permission comes from the user (A3).
//!
//! A call may start under a live approval command (`Grant::User`) or under
//! a rule the user wrote in advance in the rules file, `v.toml`
//! (`Grant::Rule`). A rule names a tool, optionally the actor's own name,
//! and a pattern its subject must match (a path, a command). Only `allow`
//! rules exist: a call no rule matches asks the user. The rules in force
//! are recorded in the log (`RulesLoaded`), and a rule grant names the
//! record that holds them, so the log alone says why each call started.
//!
//! These are the types and their views; the matching function is the
//! core's (`verified_core::rules`), where it is proved.

#![expect(
    clippy::indexing_slicing,
    reason = "Verus proves every index in this module is in bounds"
)]
#![expect(
    clippy::arithmetic_side_effects,
    reason = "Verus proves no arithmetic in this module overflows"
)]

use core::fmt;
use core::str::FromStr;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::{self, Serialize, Serializer};
use vstd::prelude::*;

use crate::record::Tool;

verus! {

/// An actor's name: 1 to 64 bytes of `[A-Za-z0-9_-]`, parsed at the
/// boundary. A rule may name one; who spawned an actor never matters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub struct ActorName(Vec<u8>);

impl View for ActorName {
    type V = Seq<u8>;

    closed spec fn view(&self) -> Seq<u8> {
        self.0@
    }
}

impl ActorName {
    /// The name's bytes.
    #[must_use]
    pub const fn bytes(&self) -> (bytes: &Vec<u8>)
        ensures
            bytes@ == self@,
    {
        &self.0
    }

    /// A copy with the same view.
    #[must_use]
    pub fn copy(&self) -> (copy: ActorName)
        ensures
            copy@ == self@,
    {
        ActorName(self.0.clone())
    }
}

/// What a rule's subject must be: anything, exactly this, or starting with
/// this.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Pattern {
    Any,
    Exact(#[serde(with = "utf8")] Vec<u8>),
    Prefix(#[serde(with = "utf8")] Vec<u8>),
}

/// A pattern as the proofs see it.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `Seq` values that exist only while verifying")]
pub enum PatternView {
    Any,
    Exact(Seq<u8>),
    Prefix(Seq<u8>),
}

impl View for Pattern {
    type V = PatternView;

    open spec fn view(&self) -> PatternView {
        match self {
            Pattern::Any => PatternView::Any,
            Pattern::Exact(bytes) => PatternView::Exact(bytes@),
            Pattern::Prefix(bytes) => PatternView::Prefix(bytes@),
        }
    }
}

/// One `allow` rule: calls of `tool` whose subject fits `subject`, by the
/// actor named `actor` (any actor when `None`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Rule {
    pub actor: Option<ActorName>,
    pub tool: Tool,
    pub subject: Pattern,
}

/// A rule as the proofs see it.
#[expect(missing_debug_implementations, reason = "spec-only type: it holds ghost `Seq` values that exist only while verifying")]
pub struct RuleView {
    pub actor: Option<Seq<u8>>,
    pub tool: Tool,
    pub subject: PatternView,
}

impl View for Rule {
    type V = RuleView;

    open spec fn view(&self) -> RuleView {
        RuleView {
            actor: match self.actor {
                Some(name) => Some(name@),
                None => None,
            },
            tool: self.tool,
            subject: self.subject@,
        }
    }
}

impl Rule {
    /// A copy with the same view.
    #[must_use]
    #[expect(clippy::manual_map, reason = "Verus has no closures, so no `Option::map`")]
    pub fn copy(&self) -> (copy: Rule)
        ensures
            copy@ == self@,
    {
        let actor = match &self.actor {
            Some(name) => Some(name.copy()),
            None => None,
        };
        let subject = match &self.subject {
            Pattern::Any => Pattern::Any,
            Pattern::Exact(bytes) => Pattern::Exact(bytes.clone()),
            Pattern::Prefix(bytes) => Pattern::Prefix(bytes.clone()),
        };
        Rule { actor, tool: self.tool, subject }
    }
}

/// The rules in force, in file order; the first that fits a call grants it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Rules {
    pub rules: Vec<Rule>,
}

impl View for Rules {
    type V = Seq<RuleView>;

    open spec fn view(&self) -> Seq<RuleView> {
        self.rules@.map_values(|rule: Rule| rule@)
    }
}

impl Rules {
    /// A copy with the same view.
    #[must_use]
    pub fn copy(&self) -> (copy: Rules)
        ensures
            copy@ == self@,
    {
        let mut rules: Vec<Rule> = Vec::with_capacity(self.rules.len());
        let mut index: usize = 0;
        while index < self.rules.len()
            invariant
                index <= self.rules@.len(),
                rules@.len() == index,
                forall|at: int| 0 <= at < index ==> (#[trigger] rules@[at])@ == self.rules@[at]@,
            decreases self.rules@.len() - index,
        {
            rules.push(self.rules[index].copy());
            index += 1;
        }
        let copy = Rules { rules };
        assert(copy@ =~= self@);
        copy
    }
}

/// Who granted a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub enum Grant {
    /// The user's approval command for this call.
    User,
    /// Rule `rule` of the rules recorded by the `RulesLoaded` at seq
    /// `rules`: that record is the version the grant names.
    Rule { rule: u64, rules: u64 },
}

/// Who an actor is. The first record of its log after the boot's
/// `EpochBumped`, at most once.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Born {
    /// The name rules match on.
    pub name: ActorName,
}

} // verus!

/// The longest actor name.
pub const MAX_ACTOR_NAME_BYTES: usize = 64;

/// Why text is not an actor name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("an actor name is empty")]
    Empty,
    #[error("an actor name is {length} bytes, longer than {MAX_ACTOR_NAME_BYTES}")]
    TooLong { length: usize },
    #[error("an actor name has a character outside [A-Za-z0-9_-]")]
    BadCharacter,
}

#[expect(
    clippy::multiple_inherent_impl,
    reason = "plain Rust beside the impl inside `verus!`: no proof mentions it"
)]
impl ActorName {
    /// # Errors
    /// Empty, longer than [`MAX_ACTOR_NAME_BYTES`], or a character outside
    /// `[A-Za-z0-9_-]`.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        if text.is_empty() {
            return Err(NameError::Empty);
        }
        if text.len() > MAX_ACTOR_NAME_BYTES {
            return Err(NameError::TooLong { length: text.len() });
        }
        if !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(NameError::BadCharacter);
        }
        Ok(Self(text.as_bytes().to_vec()))
    }

    /// The name as text: it is ASCII by construction.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl fmt::Display for ActorName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ActorName {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for ActorName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ActorName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

impl Default for Rules {
    /// No rules file: reads are allowed, everything else asks the user.
    fn default() -> Self {
        Self {
            rules: vec![Rule {
                actor: None,
                tool: Tool::Read,
                subject: Pattern::Any,
            }],
        }
    }
}

/// A pattern's bytes as UTF-8 text: the rules file is text.
mod utf8 {
    use super::{Deserialize as _, Deserializer, Serializer, ser};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(core::str::from_utf8(bytes).map_err(ser::Error::custom)?)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        Ok(String::deserialize(deserializer)?.into_bytes())
    }
}
