//! Identifiers: one set of newtypes for every crate.
//!
//! Entity ids (sessions, messages, turns) are `UUIDv7`s built from Clock and
//! Entropy seam data with [`uuid::Builder`], never from the convenience
//! constructors that read the clock or the RNG themselves. As text they are
//! canonical lowercase hyphenated `UUIDv7`s and only that: a non-v7, non-RFC
//! variant or differently spelled UUID does not parse, so every id has
//! exactly one spelling and no other value can be made.
//!
//! Protocol ids (tool calls, approvals, stream part ids, tool names) are short
//! ASCII tokens minted by the daemon or the model provider.
//!
//! The core's own ids (`StepId`, `CallIx`, `Epoch`, `EffectId`) are derived
//! from the log. The ids the core's proofs mention live inside `verus!` and
//! expose a `View` (the raw value) so specifications can compare them.

use core::fmt;
use core::str::FromStr;

use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, Serializer};
use uuid::{Builder, Uuid, Variant};
use vstd::prelude::*;

verus! {

/// One session (one conversation, one log), minted by the client that
/// creates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub struct SessionId(u128);

/// A user message, minted by the client and reused on every retry of the
/// send, so a repeat is answered with "exists".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub struct MessageId(u128);

/// A turn: one user message and the steps it causes. Minted by the daemon
/// (from seam data carried in the event) when a message starts one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub struct TurnId(u128);

impl View for SessionId {
    type V = u128;

    closed spec fn view(&self) -> u128 {
        self.0
    }
}

impl View for MessageId {
    type V = u128;

    closed spec fn view(&self) -> u128 {
        self.0
    }
}

impl View for TurnId {
    type V = u128;

    closed spec fn view(&self) -> u128 {
        self.0
    }
}

impl SessionId {
    /// The id minted at `at` with `entropy` from the Entropy seam.
    #[verifier::external]
    #[must_use]
    pub const fn from_parts(at: UnixMillis, entropy: &IdEntropy) -> Self {
        Self(mint(at, entropy))
    }

    /// # Errors
    /// Anything but canonical lowercase hyphenated `UUIDv7` text.
    #[verifier::external]
    pub fn parse(text: &str) -> Result<Self, IdError> {
        parse_v7(text).map(Self)
    }

    #[must_use]
    pub const fn as_u128(self) -> (value: u128)
        ensures
            value == self@,
    {
        self.0
    }
}

impl MessageId {
    /// The id minted at `at` with `entropy` from the Entropy seam.
    #[verifier::external]
    #[must_use]
    pub const fn from_parts(at: UnixMillis, entropy: &IdEntropy) -> Self {
        Self(mint(at, entropy))
    }

    /// # Errors
    /// Anything but canonical lowercase hyphenated `UUIDv7` text.
    #[verifier::external]
    pub fn parse(text: &str) -> Result<Self, IdError> {
        parse_v7(text).map(Self)
    }

    #[must_use]
    pub const fn as_u128(self) -> (value: u128)
        ensures
            value == self@,
    {
        self.0
    }

    /// Whether two ids name the same message.
    #[must_use]
    pub const fn same(self, other: Self) -> (same: bool)
        ensures
            same == (self@ == other@),
    {
        self.0 == other.0
    }
}

impl TurnId {
    /// The id minted at `at` with `entropy` from the Entropy seam.
    #[verifier::external]
    #[must_use]
    pub const fn from_parts(at: UnixMillis, entropy: &IdEntropy) -> Self {
        Self(mint(at, entropy))
    }

    /// # Errors
    /// Anything but canonical lowercase hyphenated `UUIDv7` text.
    #[verifier::external]
    pub fn parse(text: &str) -> Result<Self, IdError> {
        parse_v7(text).map(Self)
    }

    #[must_use]
    pub const fn as_u128(self) -> (value: u128)
        ensures
            value == self@,
    {
        self.0
    }

    /// Whether two ids name the same turn.
    #[must_use]
    pub const fn same(self, other: Self) -> (same: bool)
        ensures
            same == (self@ == other@),
    {
        self.0 == other.0
    }
}

/// One model call within a turn. Retries of the same call keep the id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct StepId {
    pub turn: TurnId,
    pub index: u32,
}

/// Whether two steps are the same step of the same turn.
pub open spec fn same_step(one: StepId, other: StepId) -> bool {
    one.turn@ == other.turn@ && one.index == other.index
}

impl StepId {
    #[must_use]
    pub const fn same(self, other: StepId) -> (same: bool)
        ensures
            same == same_step(self, other),
    {
        self.turn.same(other.turn) && self.index == other.index
    }
}

/// A tool call, as the model provider names it (Anthropic's `tool_use` id):
/// the idempotency key of the core-to-tool hop, reused on retry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub struct CallId(String);

impl CallId {
    /// # Errors
    /// Empty, longer than [`MAX_PROTOCOL_ID_BYTES`], or a character outside
    /// `[A-Za-z0-9_-]`.
    #[verifier::external]
    pub fn parse(text: &str) -> Result<Self, IdError> {
        protocol_id("tool call id", text).map(|()| Self(text.to_owned()))
    }

    #[verifier::external]
    #[must_use]
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether two ids name the same call.
    #[verifier::external_body]
    #[must_use]
    pub fn same(&self, other: &CallId) -> bool {
        self.0 == other.0
    }
}

/// A call's position in its turn: the index the log uses to name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct CallIx(pub usize);

/// Bumped and persisted on every recovery, so a reply to an effect issued
/// before a crash can never match one issued after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[verifier::external_derive]
pub struct Epoch(pub u64);

/// Names one effect: the epoch it was issued in and the sequence number of
/// the record that licensed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[verifier::external_derive]
pub struct EffectId {
    pub epoch: Epoch,
    pub seq: u64,
}

impl EffectId {
    #[must_use]
    pub const fn same(self, other: EffectId) -> (same: bool)
        ensures
            same == (self.epoch.0 == other.epoch.0 && self.seq == other.seq),
    {
        self.epoch.0 == other.epoch.0 && self.seq == other.seq
    }
}

} // verus!

/// Why text is not an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("timestamp {millis} ms does not fit the 48 bits of a UUIDv7")]
    TimestampOutOfRange { millis: u64 },
    #[error("not a canonical lowercase hyphenated UUID")]
    NotCanonical,
    #[error("UUID version {version} where version 7 was expected")]
    NotVersion7 { version: usize },
    #[error("UUID variant is not RFC 9562")]
    NotRfcVariant,
    #[error("{kind} is empty")]
    Empty { kind: &'static str },
    #[error("{kind} is {length} bytes, longer than {limit}")]
    TooLong {
        kind: &'static str,
        length: usize,
        limit: usize,
    },
    #[error("{kind} has a character outside [A-Za-z0-9_-]")]
    BadCharacter { kind: &'static str },
}

/// Milliseconds since the Unix epoch, within the 48 bits a `UUIDv7` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixMillis(u64);

impl UnixMillis {
    /// The largest timestamp a `UUIDv7` can carry.
    pub const MAX: u64 = 0xFFFF_FFFF_FFFF;

    /// # Errors
    /// [`IdError::TimestampOutOfRange`] past [`UnixMillis::MAX`].
    pub const fn new(millis: u64) -> Result<Self, IdError> {
        if millis > Self::MAX {
            Err(IdError::TimestampOutOfRange { millis })
        } else {
            Ok(Self(millis))
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Bytes from the Entropy seam that fill a `UUIDv7`'s random bits.
pub type IdEntropy = [u8; 10];

/// The length of a UUID's canonical text form.
const UUID_TEXT_LEN: usize = 36;

const fn mint(at: UnixMillis, entropy: &IdEntropy) -> u128 {
    Builder::from_unix_timestamp_millis(at.get(), entropy)
        .into_uuid()
        .as_u128()
}

/// Canonical lowercase hyphenated `UUIDv7` text to its value.
fn parse_v7(text: &str) -> Result<u128, IdError> {
    if text.len() != UUID_TEXT_LEN {
        return Err(IdError::NotCanonical);
    }
    let uuid = Uuid::try_parse(text).map_err(|_parse| IdError::NotCanonical)?;
    let mut buffer = [0u8; UUID_TEXT_LEN];
    if uuid.hyphenated().encode_lower(&mut buffer) != text {
        return Err(IdError::NotCanonical);
    }
    if uuid.get_variant() != Variant::RFC4122 {
        return Err(IdError::NotRfcVariant);
    }
    match uuid.get_version_num() {
        7 => Ok(uuid.as_u128()),
        version => Err(IdError::NotVersion7 { version }),
    }
}

fn write_uuid(value: u128, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut buffer = [0u8; UUID_TEXT_LEN];
    formatter.write_str(
        Uuid::from_u128(value)
            .hyphenated()
            .encode_lower(&mut buffer),
    )
}

macro_rules! uuid7_text {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_uuid(self.as_u128(), formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::parse(text)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Text;

                impl Visitor<'_> for Text {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a canonical lowercase hyphenated UUIDv7")
                    }

                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$name, E> {
                        $name::parse(v).map_err(E::custom)
                    }
                }

                deserializer.deserialize_str(Text)
            }
        }
    };
}

uuid7_text!(SessionId);
uuid7_text!(MessageId);
uuid7_text!(TurnId);

impl fmt::Display for EffectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch.0, self.seq)
    }
}

/// The longest protocol id accepted.
pub const MAX_PROTOCOL_ID_BYTES: usize = 128;

/// Parses a protocol id: 1 to [`MAX_PROTOCOL_ID_BYTES`] of `[A-Za-z0-9_-]`.
fn protocol_id(kind: &'static str, text: &str) -> Result<(), IdError> {
    if text.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if text.len() > MAX_PROTOCOL_ID_BYTES {
        return Err(IdError::TooLong {
            kind,
            length: text.len(),
            limit: MAX_PROTOCOL_ID_BYTES,
        });
    }
    if text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(())
    } else {
        Err(IdError::BadCharacter { kind })
    }
}

/// The text impls every protocol id shares.
macro_rules! protocol_text {
    ($name:ident) => {
        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::parse(&text)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                Self::parse(&text).map_err(de::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::parse(text)
            }
        }
    };
}

/// A protocol id the core never sees.
macro_rules! protocol_id {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// # Errors
            /// Empty, longer than [`MAX_PROTOCOL_ID_BYTES`], or a character
            /// outside `[A-Za-z0-9_-]`.
            pub fn parse(text: &str) -> Result<Self, IdError> {
                protocol_id($kind, text).map(|()| Self(text.to_owned()))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        protocol_text!($name);
    };
}

protocol_text!(CallId);
protocol_id!(
    /// An approval request. The daemon uses the tool call id's text.
    ApprovalId,
    "approval id"
);
protocol_id!(
    /// A text or reasoning part within one streamed message (`t0`, `r1`).
    PartId,
    "part id"
);
protocol_id!(
    /// A tool's name (`read`, `edit`, `bash`).
    ToolName,
    "tool name"
);

impl From<&CallId> for ApprovalId {
    /// The approval request for a call is named by the call's id.
    fn from(call: &CallId) -> Self {
        Self(call.as_str().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        ApprovalId, CallId, IdError, MAX_PROTOCOL_ID_BYTES, MessageId, PartId, SessionId, ToolName,
        TurnId, UnixMillis,
    };

    #[test]
    fn builds_a_v7_from_seam_data() {
        let at = UnixMillis::new(0x0190_0000_0000).expect("a 48-bit timestamp");
        let id = MessageId::from_parts(at, &[0xAB; 10]);
        assert_eq!((id.as_u128() >> 76u32) & 0xF, 7, "version 7");
        assert_eq!(id.to_string(), "01900000-0000-7bab-abab-abababababab");
    }

    #[test]
    fn rejects_timestamps_past_48_bits() {
        let past = UnixMillis::MAX + 1;
        assert_eq!(
            UnixMillis::new(past),
            Err(IdError::TimestampOutOfRange { millis: past })
        );
    }

    #[test]
    fn parses_only_the_canonical_spelling_of_a_v7() {
        let text = "01a10400-0000-7000-8000-000000000001";
        assert_eq!(
            TurnId::parse(text).map(|id| id.to_string()),
            Ok(text.to_owned())
        );
        for bad in [
            "01A10400-0000-7000-8000-000000000001",
            "01a1040000007000800000000000000001",
            "{01a10400-0000-7000-8000-000000000001}",
            "01a10400-0000-7000-8000-00000000000",
        ] {
            assert_eq!(TurnId::parse(bad), Err(IdError::NotCanonical), "{bad}");
        }
        assert_eq!(
            TurnId::parse("01a10400-0000-4000-8000-000000000001"),
            Err(IdError::NotVersion7 { version: 4 })
        );
        assert_eq!(
            TurnId::parse("01a10400-0000-7000-c000-000000000001"),
            Err(IdError::NotRfcVariant)
        );
        assert!(
            serde_json::from_str::<TurnId>("\"01a10400-0000-4000-8000-000000000001\"").is_err(),
            "deserializing checks the version too"
        );
    }

    #[test]
    fn protocol_ids_are_short_ascii_tokens() {
        let token = "toolu_01A09q90qw90lq917835lq9";
        assert_eq!(
            CallId::parse(token).map(|id| id.to_string()),
            Ok(token.to_owned())
        );
        assert_eq!(
            CallId::parse(""),
            Err(IdError::Empty {
                kind: "tool call id"
            })
        );
        assert_eq!(
            PartId::parse("t 0"),
            Err(IdError::BadCharacter { kind: "part id" })
        );
        assert!(matches!(
            ToolName::parse(&"x".repeat(MAX_PROTOCOL_ID_BYTES + 1)),
            Err(IdError::TooLong { .. })
        ));
        let call = CallId::parse("toolu_1").expect("an id");
        assert_eq!(ApprovalId::from(&call).as_str(), "toolu_1");
        assert!(
            serde_json::from_str::<CallId>("\"a.b\"").is_err(),
            "deserializing parses"
        );
    }

    proptest! {
        #[test]
        fn minted_ids_round_trip_through_text_and_json(
            millis in 0..=UnixMillis::MAX,
            entropy in any::<[u8; 10]>(),
        ) {
            let at = UnixMillis::new(millis).expect("in range");
            let id = SessionId::from_parts(at, &entropy);
            prop_assert_eq!(SessionId::parse(&id.to_string()), Ok(id));
            let json = serde_json::to_string(&id).expect("ids serialize");
            prop_assert_eq!(serde_json::from_str::<SessionId>(&json).ok(), Some(id));
        }

        #[test]
        fn minted_ids_sort_by_time(
            earlier in 0..UnixMillis::MAX,
            entropy_a in any::<[u8; 10]>(),
            entropy_b in any::<[u8; 10]>(),
        ) {
            let later = earlier.checked_add(1).expect("below MAX");
            let first = TurnId::from_parts(UnixMillis::new(earlier).expect("in range"), &entropy_a);
            let second = TurnId::from_parts(UnixMillis::new(later).expect("in range"), &entropy_b);
            prop_assert!(first < second);
        }

        #[test]
        fn parsing_never_panics_and_round_trips(text in "\\PC{0,40}") {
            if let Ok(id) = MessageId::parse(&text) {
                prop_assert_eq!(id.to_string(), text);
            }
        }

        #[test]
        fn protocol_ids_round_trip(text in "[A-Za-z0-9_-]{1,128}") {
            let id = CallId::parse(&text).expect("a valid token");
            let json = serde_json::to_string(&id).expect("ids serialize");
            prop_assert_eq!(serde_json::from_str::<CallId>(&json).ok(), Some(id));
        }
    }
}
