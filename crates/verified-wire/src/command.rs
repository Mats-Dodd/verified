//! Commands the TUI sends the daemon, each carrying a client-minted id.
//!
//! Every command is a POST body. The sender mints the id, persists it and
//! reuses it on retry; the daemon answers a repeat with [`Outcome::Exists`]
//! (`TigerBeetle`'s "exists"), and a repeat whose content differs from the
//! original with a [`Rejection`]. Bodies are parsed once, here, against
//! [`WireLimits`]; unknown fields are rejected because both ends are the same
//! binary.
//!
//! Routes (the server owns them; listed so both ends agree):
//! `POST /sessions` [`CreateSession`], `POST /sessions/{id}/messages`
//! [`SendMessage`], `POST /sessions/{id}/approvals` [`Approve`],
//! `POST /sessions/{id}/cancel` [`Cancel`].

use core::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::WireLimits;
use verified_domain::id::{ActorId, CallId, MessageId, TurnId};

/// Why a command body did not parse.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("body is {length} bytes, longer than {limit}")]
    TooLarge { length: usize, limit: usize },
    #[error("malformed body: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Text(#[from] TextError),
}

/// Why text is not a [`MessageText`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("message text is empty")]
    Empty,
    #[error("message text is {length} bytes, longer than {limit}")]
    TooLong { length: usize, limit: usize },
}

/// What the user typed: non-empty and within `max_message_text_bytes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct MessageText(String);

impl MessageText {
    /// # Errors
    /// Empty text, or text longer than `limits.max_message_text_bytes`.
    pub fn parse(text: String, limits: &WireLimits) -> Result<Self, TextError> {
        if text.is_empty() {
            return Err(TextError::Empty);
        }
        if text.len() > limits.max_message_text_bytes {
            return Err(TextError::TooLong {
                length: text.len(),
                limit: limits.max_message_text_bytes,
            });
        }
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Decodes a JSON body no larger than `limits.max_body_bytes`.
fn json_body<T: DeserializeOwned>(body: &[u8], limits: &WireLimits) -> Result<T, DecodeError> {
    if body.len() > limits.max_body_bytes {
        return Err(DecodeError::TooLarge {
            length: body.len(),
            limit: limits.max_body_bytes,
        });
    }
    Ok(serde_json::from_slice(body)?)
}

/// Encodes a body; the types here always serialize.
fn json_of<T: Serialize>(body: &T) -> Vec<u8> {
    serde_json::to_vec(body).expect("wire types have string keys and serialize infallibly")
}

/// Creates a session with a client-minted id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateSession {
    pub session_id: ActorId,
}

impl CreateSession {
    /// # Errors
    /// An oversized or malformed body.
    pub fn decode(body: &[u8], limits: &WireLimits) -> Result<Self, DecodeError> {
        json_body(body, limits)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        json_of(self)
    }
}

/// Sends a user message; starts a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessage {
    pub message_id: MessageId,
    pub text: MessageText,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawSendMessage {
    message_id: MessageId,
    text: String,
}

impl SendMessage {
    /// # Errors
    /// An oversized or malformed body, or text that is not a [`MessageText`].
    pub fn decode(body: &[u8], limits: &WireLimits) -> Result<Self, DecodeError> {
        let raw: RawSendMessage = json_body(body, limits)?;
        Ok(Self {
            message_id: raw.message_id,
            text: MessageText::parse(raw.text, limits)?,
        })
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        json_of(self)
    }
}

/// The user's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Grant,
    Deny,
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Grant => "granted",
            Self::Deny => "denied",
        })
    }
}

/// Answers the approval request of one tool call. The call id is the
/// `TigerBeetle` id: answering twice the same way is "exists".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Approve {
    pub call_id: CallId,
    pub verdict: Verdict,
}

impl Approve {
    /// # Errors
    /// An oversized or malformed body.
    pub fn decode(body: &[u8], limits: &WireLimits) -> Result<Self, DecodeError> {
        json_body(body, limits)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        json_of(self)
    }
}

/// Cancels the named turn. The turn id is required: a cancel is aimed at
/// the turn the user saw running, never at whichever turn runs when the
/// command lands (a retried cancel must not stop a later turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cancel {
    pub turn_id: TurnId,
}

impl Cancel {
    /// # Errors
    /// An oversized or malformed body.
    pub fn decode(body: &[u8], limits: &WireLimits) -> Result<Self, DecodeError> {
        json_body(body, limits)
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        json_of(self)
    }
}

/// Whether a command did something new or repeated one already done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The command took effect now.
    Created,
    /// The same command (same id, same content) already took effect; nothing changed.
    Exists,
}

/// Reply to [`CreateSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateSessionReply {
    pub outcome: Outcome,
}

/// Reply to [`SendMessage`]: the turn the message started (the same turn on a repeat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessageReply {
    pub turn_id: TurnId,
    pub outcome: Outcome,
}

/// Reply to [`Approve`]: the verdict is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApproveReply {
    pub outcome: Outcome,
}

/// Reply to [`Cancel`]: the turn being cancelled. [`Outcome::Exists`] when it
/// was already being cancelled. The turn ends with whatever actually happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelReply {
    pub turn_id: TurnId,
    pub outcome: Outcome,
}

/// Decodes a reply body (the TUI's side).
///
/// # Errors
/// An oversized or malformed body.
pub fn decode_reply<T: DeserializeOwned>(
    body: &[u8],
    limits: &WireLimits,
) -> Result<T, DecodeError> {
    json_body(body, limits)
}

/// Encodes a reply or rejection body (the daemon's side).
#[must_use]
pub fn encode_reply<T: Serialize>(reply: &T) -> Vec<u8> {
    json_of(reply)
}

/// A command the daemon refused. Serialized as the error body, tagged by `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(
    tag = "error",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Rejection {
    #[error("the body did not parse: {reason}")]
    Malformed { reason: String },
    #[error("no session {session_id}")]
    SessionNotFound { session_id: ActorId },
    #[error("session {session_id} exists")]
    SessionExists { session_id: ActorId },
    #[error("session {session_id} is running turn {turn_id}")]
    SessionBusy {
        session_id: ActorId,
        turn_id: TurnId,
    },
    #[error("message {message_id} exists with different text")]
    MessageExistsWithDifferentText { message_id: MessageId },
    #[error("tool call {call_id} is not awaiting approval")]
    NoPendingApproval { call_id: CallId },
    #[error("tool call {call_id} was already {verdict}")]
    ApprovalExistsWithDifferentVerdict { call_id: CallId, verdict: Verdict },
    #[error("turn {turn_id} is not running")]
    TurnNotRunning { turn_id: TurnId },
    /// The session holds all it may (turns or bytes); it takes no new message.
    #[error("session {session_id} is full: {reason}")]
    SessionFull { session_id: ActorId, reason: String },
    /// The daemon hosts its limit of sessions.
    #[error("the daemon hosts its limit of {limit} sessions")]
    TooManySessions { limit: usize },
    /// The session, or the daemon, has its limit of live stream followers;
    /// retry later.
    #[error("too many live readers of session {session_id} (limit {limit})")]
    TooManyFollowers { session_id: ActorId, limit: usize },
}

impl Rejection {
    /// The HTTP status the daemon answers with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::Malformed { .. } => 400,
            Self::SessionNotFound { .. } => 404,
            Self::SessionExists { .. }
            | Self::SessionBusy { .. }
            | Self::MessageExistsWithDifferentText { .. }
            | Self::NoPendingApproval { .. }
            | Self::ApprovalExistsWithDifferentVerdict { .. }
            | Self::TurnNotRunning { .. }
            | Self::SessionFull { .. } => 409,
            Self::TooManySessions { .. } | Self::TooManyFollowers { .. } => 503,
        }
    }
}

impl From<DecodeError> for Rejection {
    fn from(error: DecodeError) -> Self {
        Self::Malformed {
            reason: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: WireLimits = WireLimits::DEFAULT;

    #[test]
    fn send_message_round_trips() {
        let body = br#"{"messageId":"01a10400-0000-7000-8000-000000000001","text":"hi"}"#;
        let message = SendMessage::decode(body, &LIMITS).expect("a valid body");
        assert_eq!(message.text.as_str(), "hi");
        assert_eq!(message.encode(), body.to_vec());
    }

    #[test]
    fn send_message_rejects_empty_long_and_unknown() {
        let empty = br#"{"messageId":"01a10400-0000-7000-8000-000000000001","text":""}"#;
        assert!(matches!(
            SendMessage::decode(empty, &LIMITS),
            Err(DecodeError::Text(TextError::Empty))
        ));
        let tight = WireLimits {
            max_message_text_bytes: 1,
            ..LIMITS
        };
        let long = br#"{"messageId":"01a10400-0000-7000-8000-000000000001","text":"ab"}"#;
        assert!(matches!(
            SendMessage::decode(long, &tight),
            Err(DecodeError::Text(TextError::TooLong {
                length: 2,
                limit: 1
            }))
        ));
        let extra = br#"{"messageId":"01a10400-0000-7000-8000-000000000001","text":"a","x":1}"#;
        assert!(matches!(
            SendMessage::decode(extra, &LIMITS),
            Err(DecodeError::Json(_))
        ));
        let small = WireLimits {
            max_body_bytes: 4,
            ..LIMITS
        };
        assert!(matches!(
            SendMessage::decode(long, &small),
            Err(DecodeError::TooLarge { limit: 4, .. })
        ));
    }

    #[test]
    fn cancel_names_its_turn() {
        assert!(
            matches!(Cancel::decode(b"{}", &LIMITS), Err(DecodeError::Json(_))),
            "a cancel without a turn id does not parse"
        );
        let cancel = Cancel { turn_id: TurnId(7) };
        let body = cancel.encode();
        assert_eq!(body, br#"{"turnId":"7"}"#.to_vec());
        assert_eq!(Cancel::decode(&body, &LIMITS).ok(), Some(cancel));
    }

    #[test]
    fn capacity_rejections_are_typed() {
        let full = Rejection::TooManySessions { limit: 2 };
        assert_eq!(full.status(), 503, "retryable");
        assert_eq!(
            encode_reply(&full),
            br#"{"error":"too-many-sessions","limit":2}"#.to_vec()
        );
        assert_eq!(
            decode_reply::<Rejection>(&encode_reply(&full), &LIMITS).ok(),
            Some(full)
        );
    }

    #[test]
    fn approve_and_rejections_have_stable_json() {
        let approve = Approve {
            call_id: CallId::parse("toolu_1").expect("a token"),
            verdict: Verdict::Grant,
        };
        assert_eq!(
            approve.encode(),
            br#"{"callId":"toolu_1","verdict":"grant"}"#.to_vec()
        );
        let rejection = Rejection::ApprovalExistsWithDifferentVerdict {
            call_id: approve.call_id,
            verdict: Verdict::Deny,
        };
        let body = encode_reply(&rejection);
        assert_eq!(
            body,
            br#"{"error":"approval-exists-with-different-verdict","callId":"toolu_1","verdict":"deny"}"#
                .to_vec()
        );
        assert_eq!(
            decode_reply::<Rejection>(&body, &LIMITS).ok(),
            Some(rejection)
        );
    }

    #[test]
    fn replies_say_exists_on_repeat() {
        let reply = SendMessageReply {
            turn_id: TurnId(7),
            outcome: Outcome::Exists,
        };
        let body = encode_reply(&reply);
        assert_eq!(body, br#"{"turnId":"7","outcome":"exists"}"#.to_vec());
        assert_eq!(decode_reply(&body, &LIMITS).ok(), Some(reply));
    }
}
