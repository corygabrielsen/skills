//! Stall comparator key.
//!
//! `run_loop` detects stalls by comparing the *current* iteration's
//! action against the *previous* non-`Wait` action on two fields:
//! its kind and its `BlockerKey`. Other fields (urgency, description,
//! automation, target_effect) intentionally don't participate —
//! they're derived from kind+blocker plus per-iteration observation,
//! and equality on them would either be redundant or would cause
//! spurious "not stalled" verdicts when only the description text
//! drifted.
//!
//! [`StallKey<K>`] makes the "compare on `(kind, blocker)`" rule
//! the type rather than a comment. The runner stores
//! `Option<StallKey<K>>` instead of `Option<Action<K>>`, and the
//! comparator becomes plain `==`. Adding a future field to
//! `Action<K>` no longer requires re-reading the comparator's
//! `PartialEq` projection rule.

use crate::action::Action;
use crate::blocker::BlockerKey;

/// The pair the stall comparator inspects: `(kind, blocker)`.
/// Equality on `StallKey<K>` IS the stall test.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StallKey<K> {
    pub kind: K,
    pub blocker: BlockerKey,
}

impl<K: Clone> Action<K> {
    /// Project the action down to the pair `run_loop` compares
    /// across iterations. `K: Clone` is the same bound `Action<K>`
    /// already requires for its derived `Clone` impl, so any
    /// type that can be put in an `Action` can produce a
    /// `StallKey`.
    pub fn stall_key(&self) -> StallKey<K> {
        StallKey {
            kind: self.kind.clone(),
            blocker: self.blocker.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Automation, TargetEffect, Urgency};
    use serde::Serialize;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    enum K {
        Foo,
        Bar,
    }

    fn action_with(kind: K, blocker: &str, desc: &str) -> Action<K> {
        Action {
            kind,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            payload: crate::ActionPayload::Logged(desc.into()),
            blocker: BlockerKey::tag(blocker),
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
    fn stall_key_carries_both_fields() {
        let a = action_with(K::Foo, "t1", "x");
        let key = a.stall_key();
        assert_eq!(key.kind, K::Foo);
        assert_eq!(key.blocker.as_str(), "t1");
    }
}
