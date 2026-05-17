//! Stall comparator key.
//!
//! The stall detector compares the current iteration's action
//! against the previous non-`Wait` action on exactly two fields:
//! the action-kind discriminant and the [`BlockerKey`]. All other
//! `Action` fields are deliberately excluded — they are either
//! redundant with `(kind, blocker)` or drift independently per
//! iteration without changing the underlying gate.
//!
//! # Why the discriminant, not the typed `K`
//!
//! Equality on the full per-variant payload is unsound for the
//! stall test: any payload-bearing variant whose payload trims or
//! reorders between iterations (collections, counts) would compare
//! unequal even when the underlying gate is unchanged, blinding
//! the detector. [`StallKey`] projects `kind.name()` — a stable
//! per-variant `&'static str` — and routes within-variant
//! distinctions through [`BlockerKey`] instead. The convention
//! "if you need to distinguish, push it into the blocker" is
//! enforced by the type, not by per-variant discipline.

use crate::action::{Action, ActionKindName};
use crate::blocker::BlockerKey;

/// The pair the stall comparator inspects: `(kind_name, blocker)`.
/// Equality on `StallKey` IS the stall test. Non-generic — the
/// kind discriminant is projected via [`ActionKindName::name`] to
/// a stable per-variant `&'static str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StallKey {
    pub kind_name: &'static str,
    pub blocker: BlockerKey,
}

impl<K: ActionKindName> Action<K> {
    /// Project to the stall comparator's input pair. Payload-free
    /// by construction.
    pub fn stall_key(&self) -> StallKey {
        StallKey {
            kind_name: self.kind.name(),
            blocker: self.blocker.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ActionEffect, MidTier, TargetEffect, Urgency};
    use serde::Serialize;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    enum K {
        Foo,
        Bar,
        FooWithPayload(u32),
    }

    impl ActionKindName for K {
        fn name(&self) -> &'static str {
            match self {
                Self::Foo | Self::FooWithPayload(_) => "Foo",
                Self::Bar => "Bar",
            }
        }
    }

    fn action_with(kind: K, blocker: &str, desc: &str) -> Action<K> {
        Action {
            kind,
            effect: ActionEffect::Full { log: desc.into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::for_test(blocker),
        }
    }

    #[test]
    fn equal_kind_and_blocker_yields_equal_stall_keys() {
        let a = action_with(K::Foo, "t1", "alpha");
        let b = action_with(K::Foo, "t1", "beta"); // different description
        assert_eq!(a.stall_key(), b.stall_key());
    }

    #[test]
    fn different_kind_yields_distinct_stall_keys() {
        let a = action_with(K::Foo, "t1", "x");
        let b = action_with(K::Bar, "t1", "x");
        assert_ne!(a.stall_key(), b.stall_key());
    }

    #[test]
    fn different_blocker_yields_distinct_stall_keys() {
        let a = action_with(K::Foo, "t1", "x");
        let b = action_with(K::Foo, "t2", "x");
        assert_ne!(a.stall_key(), b.stall_key());
    }

    #[test]
    fn same_variant_different_payload_yields_equal_stall_keys() {
        // Payload-bearing variants must collapse to the same stall
        // key when the gate is unchanged — otherwise drift in the
        // payload (counts, collections) would blind the detector.
        let a = action_with(K::FooWithPayload(3), "same_gate", "x");
        let b = action_with(K::FooWithPayload(5), "same_gate", "x");
        assert_eq!(a.stall_key(), b.stall_key());
    }

    #[test]
    fn stall_key_carries_both_fields() {
        let a = action_with(K::Foo, "t1", "x");
        let key = a.stall_key();
        assert_eq!(key.kind_name, "Foo");
        assert_eq!(key.blocker.as_str(), "t1");
    }
}
