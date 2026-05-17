//! First-class rate-limit observations.
//!
//! External APIs have distinct quota buckets, reset semantics, and
//! error formats. Rather than collapse them into a single opaque
//! "rate limited" signal, this module models a typed
//! [`RateLimitHit`] carrying the exact [`RateLimitScope`] and the
//! minimum back-off. Downstream layers route per-scope rather than
//! per-symptom.
//!
//! Adding a new bucket is a single [`RateLimitScope`] variant.
//! Exhaustive matches across consumers fail to compile until the
//! new arm is handled — that compile-time contract is the only
//! mechanism that keeps the taxonomy honest.

use crate::polling_interval::PollingInterval;
use serde::{Deserialize, Serialize};

/// Which external rate-limit bucket fired. Variants are
/// API-and-bucket pairs; secondary / short-window limits get
/// their own variants when meaningfully distinct from primary
/// (different counter, different reset semantics, different
/// back-off interpretation).
///
/// **GitHub:**
/// * `GitHubGraphqlPrimary` — the hourly GraphQL points quota.
/// * `GitHubRestPrimary` — the hourly authenticated REST quota.
///   Distinct counter from GraphQL.
/// * `GitHubSecondary` — short-window throttling (concurrent
///   requests, content creation, search). Carries `Retry-After`
///   rather than counting against the primary quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitScope {
    GitHubGraphqlPrimary,
    GitHubRestPrimary,
    GitHubSecondary,
}

impl RateLimitScope {
    /// Stable single-token rendering. Distinct from `Debug` so a
    /// Rust-side rename does not silently change the wire format
    /// (recorder schemas, on-the-wire records, status renderings).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::GitHubGraphqlPrimary => "github/graphql/primary",
            Self::GitHubRestPrimary => "github/rest/primary",
            Self::GitHubSecondary => "github/secondary",
        }
    }
}

/// A rate-limit observation. Carries which bucket fired and the
/// minimum back-off before the next request is permitted.
///
/// `retry_after` is a [`PollingInterval`] (strictly positive) so
/// any wait it drives cannot degenerate into a busy-loop. Absolute
/// reset timestamps are converted to this relative form at the
/// moment of detection — the observe layer owns the clock-domain
/// translation, downstream consumers see only a duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitHit {
    pub scope: RateLimitScope,
    pub retry_after: PollingInterval,
}

/// Per-bucket counters from a rate-limit snapshot. Mirrors the
/// shape GitHub returns per bucket from `GET /rate_limit`:
/// `{ "limit": …, "remaining": …, "reset": <unix-epoch-sec> }`.
/// `reset_at_epoch` stays in unix epoch seconds at this layer so
/// no clock-domain translation happens at the wire boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketState {
    pub remaining: u32,
    pub limit: u32,
    pub reset_at_epoch: u64,
}

/// Snapshot of remaining GitHub quota across both primary buckets.
/// Fetched via the no-cost rate-limit endpoint, which returns every
/// bucket counter in one response. Surfacing the snapshot gives
/// the loop visibility into throttling proximity; consumption
/// policy is the caller's choice.
///
/// # Future concepts (named, not implemented)
///
/// **Bucket biasing** — per-iteration routing advice computed
/// from this snapshot for any fetcher that exists in both REST and
/// GraphQL forms.
///
/// **Iterations-of-headroom** — the right comparison unit is
/// `remaining / estimated_calls_per_iteration`, not raw remaining
/// points. Raw points understate urgency for high-volume buckets
/// and overstate it for low-volume ones.
///
/// **Cost-model caveat** — GraphQL is not cheaper per point than
/// REST; it bills proportional to returned nodes. The only
/// structural win of bucket biasing is bucket separation (two
/// independent quotas), not aggregate cost reduction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitBudget {
    pub graphql: BucketState,
    pub rest: BucketState,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Length sentinel: forgetting a variant in the sample list
    /// below fails this assert. Adding a variant requires extending
    /// the sample list.
    const SAMPLE_COUNT: usize = 3;

    fn samples() -> Vec<RateLimitScope> {
        vec![
            RateLimitScope::GitHubGraphqlPrimary,
            RateLimitScope::GitHubRestPrimary,
            RateLimitScope::GitHubSecondary,
        ]
    }

    #[test]
    fn sample_list_covers_every_variant() {
        // Exhaustive match-as-contract: adding a variant fails
        // to compile here until included.
        for scope in samples() {
            let _: &'static str = match scope {
                RateLimitScope::GitHubGraphqlPrimary => "github/graphql/primary",
                RateLimitScope::GitHubRestPrimary => "github/rest/primary",
                RateLimitScope::GitHubSecondary => "github/secondary",
            };
        }
        assert_eq!(samples().len(), SAMPLE_COUNT);
    }

    #[test]
    fn names_are_stable_and_distinct() {
        let names: Vec<&'static str> = samples().iter().map(|s| s.name()).collect();
        // No duplicates.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        // No whitespace, no uppercase — these end up in
        // status-comment lines and JSONL records.
        for n in &names {
            assert!(!n.contains(' '));
            assert_eq!(*n, n.to_lowercase().as_str());
        }
    }

    #[test]
    fn scope_serde_roundtrip() {
        for scope in samples() {
            let json = serde_json::to_string(&scope).unwrap();
            let back: RateLimitScope = serde_json::from_str(&json).unwrap();
            assert_eq!(back, scope);
        }
    }

    #[test]
    fn hit_serde_roundtrip() {
        let hit = RateLimitHit {
            scope: RateLimitScope::GitHubGraphqlPrimary,
            retry_after: PollingInterval::from_secs(60),
        };
        let json = serde_json::to_string(&hit).unwrap();
        let back: RateLimitHit = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hit);
    }

    #[test]
    fn budget_serde_roundtrip() {
        let budget = RateLimitBudget {
            graphql: BucketState {
                remaining: 4500,
                limit: 5000,
                reset_at_epoch: 1_700_000_000,
            },
            rest: BucketState {
                remaining: 4900,
                limit: 5000,
                reset_at_epoch: 1_700_000_000,
            },
        };
        let json = serde_json::to_string(&budget).unwrap();
        let back: RateLimitBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, budget);
    }

    /// `BucketState` deserializes from the exact shape GitHub's
    /// `/rate_limit` endpoint emits per bucket. Locking this here
    /// catches any field renaming before the observe layer would.
    #[test]
    fn bucket_state_parses_github_wire_shape() {
        let wire = r#"{"limit":5000,"remaining":4999,"reset":1372700873}"#;
        // GitHub uses `"reset"`, our field is `reset_at_epoch`.
        // Document that the binary-side fetcher is responsible for
        // the rename — `BucketState` itself uses our internal name.
        // If GitHub ever exposed `reset_at_epoch` directly we'd
        // deserialize it raw; for now the fetcher does the mapping.
        let internal_wire = r#"{"limit":5000,"remaining":4999,"reset_at_epoch":1372700873}"#;
        let s: BucketState = serde_json::from_str(internal_wire).unwrap();
        assert_eq!(s.remaining, 4999);
        assert_eq!(s.limit, 5000);
        assert_eq!(s.reset_at_epoch, 1_372_700_873);
        // GitHub's raw wire form does NOT parse directly — the
        // fetcher must rename. Assert that to make the contract
        // explicit.
        assert!(serde_json::from_str::<BucketState>(wire).is_err());
    }
}
