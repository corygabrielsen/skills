//! Cohort identifiers for stall-key construction.
//!
//! Some axes key their blocker on the artifact the gate is bound to
//! — e.g. a remote head SHA, an attestation cohort, a PR-side
//! commit batch — rather than a per-iteration count or progress
//! marker. The newtypes here lift those identifiers into types that
//! implement [`GateIdentity`], so [`BlockerKey::typed`] structurally
//! accepts them as the stability witness.
//!
//! Membership is "the identifier is bound to the gate's identity": a
//! transition to a new gate cohort produces a distinct key, and the
//! same cohort produces the same key across iterations. Adding a new
//! cohort newtype here is a per-axis design decision; the type
//! system is the audit boundary.
//!
//! `Display` writes the inner identifier verbatim. The renderer is
//! the projection [`BlockerKey::typed`] reads, so the rendered form
//! is the stability witness.

use crate::blocker::GateIdentity;
use serde::Serialize;
use std::fmt;

/// A git-SHA cohort identifier. Carrier for axes whose gate is "the
/// state observed at HEAD `<sha>`" — the same SHA across two
/// iterations is the same gate; a SHA change is a fresh gate.
///
/// The validating constructor lives on the producer (the axis that
/// reads the SHA from upstream); this newtype's contract is the
/// gate-stability witness alone. Construction is unchecked because
/// every reachable producer already parses the SHA into a typed
/// form before lifting it into a cohort.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct CohortSha(String);

impl CohortSha {
    /// Lift a SHA into a cohort identifier. The caller is responsible
    /// for the SHA being well-formed; this type's contract is only
    /// the [`GateIdentity`] stability witness, not SHA validation.
    #[must_use]
    pub fn new(sha: impl Into<String>) -> Self {
        Self(sha.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CohortSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl GateIdentity for CohortSha {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker::BlockerKey;

    #[test]
    fn display_writes_inner_sha_verbatim() {
        let c = CohortSha::new("abc1234");
        assert_eq!(format!("{c}"), "abc1234");
        assert_eq!(c.as_str(), "abc1234");
    }

    #[test]
    fn equal_shas_produce_equal_keys() {
        let a = BlockerKey::typed("axis", &CohortSha::new("deadbeef"));
        let b = BlockerKey::typed("axis", &CohortSha::new("deadbeef"));
        assert_eq!(a, b, "same cohort SHA ⇒ same key");
    }

    #[test]
    fn distinct_shas_produce_distinct_keys() {
        let a = BlockerKey::typed("axis", &CohortSha::new("deadbeef"));
        let b = BlockerKey::typed("axis", &CohortSha::new("cafef00d"));
        assert_ne!(a, b, "distinct cohort SHAs ⇒ distinct keys");
    }

    #[test]
    fn distinct_categories_produce_distinct_keys() {
        let a = BlockerKey::typed("axis_a", &CohortSha::new("deadbeef"));
        let b = BlockerKey::typed("axis_b", &CohortSha::new("deadbeef"));
        assert_ne!(a, b, "same SHA in distinct categories ⇒ distinct keys");
    }
}
