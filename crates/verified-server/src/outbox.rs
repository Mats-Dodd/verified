//! Delivery between actors' logs: `send`, `spawn` and a stop.
//!
//! An envelope is delivered by appending a `Delivered` record to its
//! recipient's log, and the recipient answers only once that append is
//! durable (T1), so a sender records `Ok` only for an envelope its
//! recipient holds. The recipient deduplicates by the envelope's id, which
//! the sender minted from the seq of the record that licensed the delivery
//! (a call's `Dispatched`, a `StopSent`): a `Keyed` delivery run again
//! after a crash carries the same id and lands once (C2).
//!
//! Each delivery runs as its own task and reports to its actor's inbox as
//! any run does. A session's loop never awaits one, and a delivery awaits
//! only the registry (which awaits no session) and its recipient's bounded
//! append, so actors cannot deadlock on each other (P1). What comes back is
//! the outcome: a delivery is not cancelled mid-flight, and a `send`'s or a
//! `spawn`'s deadline bounds it (P1). A stop has none: it is sent until it
//! lands (C2), and its only waits are those of any delivery.
//!
//! Once a spawn has made its child, it has acted: whatever its prompt
//! meets, it is never `NotStarted` (T3).

use verified_core::{Outcome, Rejection};
use verified_domain::{Address, Body, Born, Envelope, Exit, Unstarted};

use crate::daemon::{CreateError, Registry};
use crate::payload::Parsed;

/// A `send`, a `spawn` or a stop to deliver.
#[derive(Debug)]
pub(crate) enum Outbound {
    /// An envelope for an actor's mailbox: a message, or a stop
    /// (`Body::Stop`).
    Send { envelope: Envelope<Parsed> },
    /// A message for you: you read it in the sender's own log, where its
    /// call already is.
    ToUser,
    /// A child to create, born as `born`, and its first message.
    Spawn {
        born: Born,
        prompt: Envelope<Parsed>,
    },
}

/// What a delivery's outcome means for its call: whether it landed, and
/// what the model is told.
fn landed(outcome: Outcome, landed: &str) -> (Exit, String) {
    match outcome {
        Outcome::Accepted | Outcome::Exists => (Exit::Ok, landed.to_owned()),
        Outcome::Rejected(Rejection::Limit) => (
            Exit::NotStarted(Unstarted::Limit),
            "not delivered: the recipient takes no more mail".to_owned(),
        ),
        Outcome::Rejected(Rejection::Stopped) => (
            Exit::NotStarted(Unstarted::Refused),
            "not delivered: the recipient is stopped".to_owned(),
        ),
        Outcome::Rejected(
            Rejection::Unborn
            | Rejection::Mismatch
            | Rejection::UnknownCall
            | Rejection::NotRunning,
        )
        | Outcome::Stale => (
            Exit::NotStarted(Unstarted::Refused),
            "not delivered: there is no such actor".to_owned(),
        ),
    }
}

/// The recipient went away mid-delivery (the daemon is stopping): its
/// append may or may not be durable.
fn lost() -> (Exit, String) {
    (
        Exit::Unknown,
        "the daemon stopped during the delivery: it may or may not have landed".to_owned(),
    )
}

/// Delivers a `send`, a `spawn` or a stop. A stop finds its child born:
/// the shell holds it until its parent's spawn of that child, if one is in
/// flight, has landed, and a child a crash left unborn can never be born
/// (its parent, stopped, reruns no spawn) and so never acts.
pub(crate) async fn deliver(registry: &Registry, work: Outbound) -> (Exit, String) {
    match work {
        Outbound::ToUser => (Exit::Ok, "delivered to user".to_owned()),
        Outbound::Send { envelope } => {
            let to = envelope.to;
            let stop = matches!(envelope.body, Body::Stop);
            let Some(handle) = registry.lookup(to).await else {
                let not = if stop { "not stopped" } else { "not delivered" };
                return (
                    Exit::NotStarted(Unstarted::Refused),
                    format!("{not}: there is no actor {to}"),
                );
            };
            let said = if stop {
                format!("stopped {to}")
            } else {
                format!("delivered to {to}")
            };
            match handle.deliver(envelope).await {
                // A child already stopped is stopped: the stop took effect.
                Ok(Outcome::Rejected(Rejection::Stopped)) if stop => (Exit::Ok, said),
                Ok(outcome) => landed(outcome, &said),
                Err(_gone) => lost(),
            }
        }
        Outbound::Spawn { born, prompt } => {
            let (child, name) = (born.actor, born.name.clone());
            let handle = match registry.spawn(born).await {
                Ok((_, handle)) => handle,
                Err(CreateError::Full { limit }) => {
                    return (
                        Exit::NotStarted(Unstarted::Limit),
                        format!("not spawned: the daemon hosts its limit of {limit} actors"),
                    );
                }
                // Its birth was refused: no actor exists that could act.
                Err(CreateError::Unborn { rejection }) => {
                    let exit = match rejection {
                        Rejection::Limit => Exit::NotStarted(Unstarted::Limit),
                        Rejection::Stopped
                        | Rejection::Unborn
                        | Rejection::Mismatch
                        | Rejection::UnknownCall
                        | Rejection::NotRunning => Exit::NotStarted(Unstarted::Refused),
                    };
                    return (exit, format!("not spawned: {name}'s birth was refused"));
                }
                // The log may or may not have been made; a rerun finds it.
                Err(error @ (CreateError::Log(_) | CreateError::Unavailable)) => {
                    return (Exit::Unknown, format!("spawning {name} failed: {error}"));
                }
            };
            let spawned = format!("spawned {name} at {}", Address::Actor(child));
            match handle.deliver(prompt).await {
                Ok(Outcome::Accepted | Outcome::Exists) => (Exit::Ok, spawned),
                Ok(Outcome::Rejected(Rejection::Stopped)) => (
                    Exit::Ok,
                    format!("{spawned}; it was stopped before its prompt landed"),
                ),
                Ok(Outcome::Rejected(Rejection::Limit)) => (
                    Exit::Ok,
                    format!("{spawned}; its prompt was not delivered: it takes no more mail"),
                ),
                Ok(
                    Outcome::Rejected(
                        Rejection::Unborn
                        | Rejection::Mismatch
                        | Rejection::UnknownCall
                        | Rejection::NotRunning,
                    )
                    | Outcome::Stale,
                ) => (Exit::Unknown, format!("{spawned}; its prompt was refused")),
                Err(_gone) => lost(),
            }
        }
    }
}
