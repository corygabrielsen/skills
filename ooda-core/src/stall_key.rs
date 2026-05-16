//! Stall comparator key.
//!
//! `run_loop` detects stalls by comparing the *current* iteration's
//! action against the *previous* non-`Wait` action on two fields:
//! its kind discriminant and its `BlockerKey`. Other fields
//! (urgency, description, automation, `target_effect`) intentionally
//! don't participate — they're derived from kind+blocker plus per-
//! iteration observation, and equality on them would either be
//! redundant or would cause spurious "not stalled" verdicts when
//! only the description text drifted.
//!
//! ## Why the discriminant, not the full `K`
//!
//! Until 2026-05 `StallKey` carried the full `K` (the per-binary
//! `ActionKind`). That made variants with `Vec` / `NonEmpty`
//! payloads — `AddressThreads { threads }`, `ReRunWorkflow
//! { checks }`, `TriageWait { blocked_checks }` — fail to compare
//! equal across iterations whenever the upstream observation
//! reordered or trimmed the payload, even though the underlying
//! gate (the `BlockerKey`) was unchanged. The stall detector
//! silently went blind for those variants and a `Full`-automation
//! axis (`ReRunWorkflow`) would re-fire every iteration until the
//! orient-side budget cap escalated it.
//!
//! [`StallKey`] now projects `kind.name()` instead — the variant
//! discriminant only, payload-free. Within-variant discrimination
//! (e.g. `ci_fail: BuildA` vs `ci_fail: BuildB`) flows through
//! [`BlockerKey`] which the decide layer already constructs with
//! that distinction. The convention "if you need to distinguish,
//! push it into the blocker" is now enforced by the stall-key
//! type, not by per-variant convention.

use crate::action::{Action, ActionKindName};
use crate::blocker::BlockerKey;

/// The pair the stall comparator inspects:
/// `(kind_name, blocker)`. Equality on `StallKey` IS the stall
/// test. Non-generic — the kind is projected via
/// [`ActionKindName::name`] to a stable `&'static str` discriminant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StallKey {
    pub kind_name: &'static str,
    pub blocker: BlockerKey,
}

impl<K: ActionKindName> Action<K> {
    /// Project the action down to the pair `run_loop` compares
    /// across iterations. The kind is projected via
    /// [`ActionKindName::name`] — a stable `&'static str` per
    /// variant, payload-free.
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
    use crate::action::{ActionEffect, TargetEffect, Urgency};
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
            urgency: Urgency::BlockingFix,
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
        // Regression: prior `StallKey<K>` would have compared the
        // payload too, marking these unequal and defeating the
        // stall detector for any variant with a varying payload.
        // The discriminant projection collapses them; the blocker
        // is the load-bearing distinguisher.
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
