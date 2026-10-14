//! The rules matcher (A2): a pure function over data in the log.
//!
//! A rule grants a call when it names the call's tool, names no actor or
//! this actor's own name, and its pattern fits the call's subject. The
//! first rule in file order that fits is the one a grant names. The
//! signature is the whole input: the rules, this actor's name and the
//! call's shape. Who spawned the actor cannot be consulted, because it is
//! not passed.

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

use verified_domain::rules::{ActorName, Pattern, Rule, Rules};
#[cfg(verus_only)]
use verified_domain::rules::{PatternView, RuleView};
use verified_domain::shape::Shape;
#[cfg(verus_only)]
use verified_domain::shape::ShapeView;

verus! {

/// Whether `pattern` fits `subject`.
pub open spec fn subject_fits(pattern: PatternView, subject: Seq<u8>) -> bool {
    match pattern {
        PatternView::Any => true,
        PatternView::Exact(bytes) => bytes == subject,
        PatternView::Prefix(bytes) => bytes.len() <= subject.len() && subject.take(
            bytes.len() as int,
        ) == bytes,
    }
}

/// Whether `rule` grants a call of `shape` by the actor named `name`
/// (`None`: an actor with no name yet).
pub open spec fn fits(rule: RuleView, name: Option<Seq<u8>>, shape: ShapeView) -> bool {
    &&& rule.tool == shape.tool
    &&& match rule.actor {
        None => true,
        Some(actor) => name == Some(actor),
    }
    &&& subject_fits(rule.subject, shape.subject)
}

/// Rule `index` is the first of `rules` that fits.
pub open spec fn first_fit(
    rules: Seq<RuleView>,
    name: Option<Seq<u8>>,
    shape: ShapeView,
    index: int,
) -> bool {
    &&& 0 <= index < rules.len()
    &&& fits(rules[index], name, shape)
    &&& forall|earlier: int| 0 <= earlier < index ==> !fits(#[trigger] rules[earlier], name, shape)
}

/// A name as the proofs see it.
pub open spec fn name_view(name: Option<&ActorName>) -> Option<Seq<u8>> {
    match name {
        Some(name) => Some(name@),
        None => None,
    }
}

/// Whether `left` and `right` hold the same bytes.
const fn same_bytes(left: &[u8], right: &[u8]) -> (same: bool)
    ensures
        same == (left@ == right@),
{
    if left.len() != right.len() {
        return false;
    }
    let mut index: usize = 0;
    while index < left.len()
        invariant
            left@.len() == right@.len(),
            index <= left@.len(),
            forall|at: int| 0 <= at < index ==> left@[at] == right@[at],
        decreases left@.len() - index,
    {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    assert(left@ =~= right@);
    true
}

/// Whether `subject` starts with `prefix`.
const fn starts_with(subject: &[u8], prefix: &[u8]) -> (starts: bool)
    ensures
        starts == (prefix@.len() <= subject@.len() && subject@.take(prefix@.len() as int)
            == prefix@),
{
    if prefix.len() > subject.len() {
        return false;
    }
    let mut index: usize = 0;
    while index < prefix.len()
        invariant
            prefix@.len() <= subject@.len(),
            index <= prefix@.len(),
            forall|at: int| 0 <= at < index ==> subject@[at] == prefix@[at],
        decreases prefix@.len() - index,
    {
        if subject[index] != prefix[index] {
            assert(subject@.take(prefix@.len() as int)[index as int] != prefix@[index as int]);
            return false;
        }
        index += 1;
    }
    assert(subject@.take(prefix@.len() as int) =~= prefix@);
    true
}

/// Whether `rule` grants a call of `shape` by the actor named `name`.
#[must_use]
pub const fn rule_fits(rule: &Rule, name: Option<&ActorName>, shape: &Shape) -> (fit: bool)
    ensures
        fit == fits(rule@, name_view(name), shape@),
{
    if !rule.tool.same(shape.tool()) {
        return false;
    }
    let actor_fits = match (&rule.actor, name) {
        (None, _) => true,
        (Some(actor), Some(name)) => same_bytes(actor.bytes().as_slice(), name.bytes().as_slice()),
        (Some(_), None) => false,
    };
    if !actor_fits {
        return false;
    }
    match (&rule.subject, shape.subject()) {
        (Pattern::Any, _) => true,
        (Pattern::Exact(bytes), Some(subject)) => same_bytes(bytes.as_slice(), subject.as_slice()),
        (Pattern::Prefix(bytes), Some(subject)) => starts_with(subject.as_slice(), bytes.as_slice()),
        // No subject: only an empty pattern fits it.
        (Pattern::Exact(bytes) | Pattern::Prefix(bytes), None) => {
            proof {
                if bytes@.len() == 0 {
                    assert(bytes@ =~= shape@.subject);
                    assert(shape@.subject.take(0) =~= bytes@);
                }
            };
            bytes.len() == 0
        },
    }
}

/// The first of `rules` that grants a call of `shape` by the actor named
/// `name`, if any does.
#[must_use]
pub fn grant(rules: &Rules, name: Option<&ActorName>, shape: &Shape) -> (found: Option<u64>)
    ensures
        match found {
            Some(index) => first_fit(rules@, name_view(name), shape@, index as int),
            None => forall|at: int|
                0 <= at < rules@.len() ==> !fits(#[trigger] rules@[at], name_view(name), shape@),
        },
{
    let mut index: usize = 0;
    while index < rules.rules.len()
        invariant
            index <= rules.rules@.len(),
            rules@.len() == rules.rules@.len(),
            forall|at: int| 0 <= at < rules@.len() ==> #[trigger] rules@[at] == rules.rules@[at]@,
            forall|at: int|
                0 <= at < index ==> !fits(#[trigger] rules@[at], name_view(name), shape@),
        decreases rules.rules@.len() - index,
    {
        if rule_fits(&rules.rules[index], name, shape) {
            #[expect(clippy::as_conversions, reason = "a usize fits in a u64 on every target")]
            let found = index as u64;
            return Some(found);
        }
        index += 1;
    }
    None
}

} // verus!
