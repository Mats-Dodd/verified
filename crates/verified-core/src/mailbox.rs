//! The mailbox: envelopes delivered and not yet consumed, in arrival order.
//!
//! A model call consumes the batch at the head of the mailbox when its
//! `StepStarted` is durable (L4): a user message alone, or the run of other
//! envelopes (agents' messages, notifications) before the next user
//! message. So a user message is never coalesced with anything, and
//! notifications that arrive together are carried together.

#![expect(
    clippy::indexing_slicing,
    reason = "Verus proves every index in this module is in bounds"
)]
#![expect(
    clippy::arithmetic_side_effects,
    reason = "Verus proves no arithmetic in this module overflows"
)]
#![expect(clippy::len_zero, reason = "vstd specifies `len`, not `is_empty`")]

use vstd::prelude::*;

use verified_domain::EnvelopeId;

verus! {

/// One envelope waiting in the mailbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[verifier::external_derive]
pub struct Waiting {
    pub id: EnvelopeId,
    /// From the user: carried alone.
    pub user: bool,
    /// What it adds to the conversation, against `Limits::context_bytes`.
    pub bytes: u64,
}

/// `n` is the size of the batch at the head of `mailbox`: nothing when it
/// is empty, a user message alone, or else every envelope before the next
/// user message.
pub open spec fn is_batch(mailbox: Seq<Waiting>, n: int) -> bool {
    if mailbox.len() == 0 {
        n == 0
    } else if mailbox[0].user {
        n == 1
    } else {
        &&& 0 < n <= mailbox.len()
        &&& forall|at: int| 0 <= at < n ==> !(#[trigger] mailbox[at]).user
        &&& (n == mailbox.len() || mailbox[n].user)
    }
}

/// The ids of the first `n` envelopes of `mailbox`.
pub open spec fn ids_of(mailbox: Seq<Waiting>, n: int) -> Seq<EnvelopeId> {
    Seq::new(n as nat, |at: int| mailbox[at].id)
}

/// Every envelope id the actor has carried or still holds, in order: what
/// may appear in its model's history, each at most once.
pub open spec fn held(consumed: Seq<EnvelopeId>, mailbox: Seq<Waiting>) -> Seq<EnvelopeId> {
    consumed + ids_of(mailbox, mailbox.len() as int)
}

/// The size of the batch at the head of `mailbox`.
#[must_use]
pub const fn batch(mailbox: &[Waiting]) -> (n: usize)
    ensures
        is_batch(mailbox@, n as int),
{
    if mailbox.len() == 0 {
        return 0;
    }
    if mailbox[0].user {
        return 1;
    }
    let mut n: usize = 1;
    while n < mailbox.len() && !mailbox[n].user
        invariant
            0 < n <= mailbox@.len(),
            !mailbox@[0].user,
            forall|at: int| 0 <= at < n ==> !(#[trigger] mailbox@[at]).user,
        decreases mailbox@.len() - n,
    {
        n += 1;
    }
    n
}

/// The ids of the first `n` envelopes of `mailbox`, and what they add to
/// the conversation (saturating: past `u64` is past any context bound).
#[must_use]
pub fn head(mailbox: &[Waiting], n: usize) -> (carried: (Vec<EnvelopeId>, u64))
    requires
        n <= mailbox@.len(),
    ensures
        carried.0@ == ids_of(mailbox@, n as int),
{
    let mut ids: Vec<EnvelopeId> = Vec::with_capacity(n);
    let mut bytes: u64 = 0;
    let mut index: usize = 0;
    while index < n
        invariant
            n <= mailbox@.len(),
            index <= n,
            ids@ == ids_of(mailbox@, index as int),
        decreases n - index,
    {
        ids.push(mailbox[index].id);
        bytes = bytes.saturating_add(mailbox[index].bytes);
        index += 1;
        assert(ids@ =~= ids_of(mailbox@, index as int));
    }
    (ids, bytes)
}

/// Whether `ids` is exactly the first `ids.len()` ids of `mailbox`.
#[must_use]
pub const fn heads(ids: &[EnvelopeId], mailbox: &[Waiting]) -> (fits: bool)
    ensures
        fits == (ids@.len() <= mailbox@.len() && ids@ == ids_of(mailbox@, ids@.len() as int)),
{
    if ids.len() > mailbox.len() {
        return false;
    }
    let mut index: usize = 0;
    while index < ids.len()
        invariant
            ids@.len() <= mailbox@.len(),
            index <= ids@.len(),
            forall|at: int| 0 <= at < index ==> #[trigger] ids@[at] == mailbox@[at].id,
        decreases ids@.len() - index,
    {
        if !ids[index].same(mailbox[index].id) {
            assert(ids@[index as int] != ids_of(mailbox@, ids@.len() as int)[index as int]);
            return false;
        }
        index += 1;
    }
    assert(ids@ =~= ids_of(mailbox@, ids@.len() as int));
    true
}

} // verus!
