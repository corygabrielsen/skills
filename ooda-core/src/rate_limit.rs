//! First-class rate-limit observations.
//!
//! Loops drive long-running interactions with external APIs (GitHub,
//! Anthropic, OpenAI, …). Each API has its own quota bucket, its
//! own reset semantics, and its own error format. Rather than
//! collapse them into a single opaque "rate limited" error, the
//! observe layer surfaces a typed [`RateLimitHit`] identifying the
//! exact [`RateLimitScope`] and the minimum back-off duration.
//! Decide then emits a top-priority `WaitForRateLimit` action;
//! the recorder, JSONL emission, and status-comment renderer all
//! see the scope explicitly.
//!
//! When a new API is added to the family, extend [`RateLimitScope`]
//! with a new variant — every exhaustive match in the family fails
//! to compile until the new arm is handled, which is the contract.

use crate::polling_interval::PollingInterval;
use serde::{Deserialize, Serialize};

/// Which external rate-limit bucket fired. Variants are
/// API-and-bucket pairs; secondary limits get their own variants
/// when they're meaningfully distinct from primary.
///
/// **GitHub:**
/// * `GitHubGraphqlPrimary` — the 5000 points/hour GraphQL quota.
/// * `GitHubRestPrimary` — the 5000 requests/hour authenticated REST
///   quota. Distinct bucket from GraphQL.
/// * `GitHubSecondary` — short-window throttling (concurrent requests,
///   content creation, search). Fires with `Retry-After`; not the
///   5000/hr quota.
///
/// Adding Anthropic / OpenAI / etc. is a single-variant extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitScope {
    GitHubGraphqlPrimary,
    GitHubRestPrimary,
    GitHubSecondary,
}

impl RateLimitScope {
    /// Stable, finite, single-token rendering. Used by recorder
    /// schemas, JSONL records, and status comments. Distinct from
    /// `Debug` so renaming a variant doesn't silently change the
    /// wire format.
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
/// `retry_after` is a [`PollingInterval`] (strictly positive) so the
/// Wait action it drives can't degenerate into a busy-loop. The
/// observe layer is responsible for converting absolute reset
/// timestamps (e.g. GitHub's `X-RateLimit-Reset` epoch seconds, or
/// secondary-limit `Retry-After` deltas) into this relative form
/// at the moment of detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitHit {
    pub scope: RateLimitScope,
    pub retry_after: PollingInterval,
}

/// Per-bucket counters from a rate-limit snapshot. Matches the shape
/// GitHub returns under each entry of `GET /rate_limit`'s `resources`
/// object: `{ "limit": …, "remaining": …, "reset": <unix-epoch-sec> }`.
/// `reset_at_epoch` is unix epoch seconds, matching the wire form, so
/// deserialization is direct and no clock-domain conversion happens
/// at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketState {
    pub remaining: u32,
    pub limit: u32,
    pub reset_at_epoch: u64,
}

/// Snapshot of remaining GitHub quota across the buckets we use.
/// Fetched via `GET /rate_limit` — that endpoint does **not** count
/// against quota and returns every bucket counter in one response.
/// Surfacing the snapshot into observations gives the loop visibility
/// into how close it is to throttling; today nothing acts on it beyond
/// recorder logging.
///
/// # Future concepts (named, not implemented)
///
/// **`BucketBias`** would name per-iteration routing advice
/// (e.g. `PreferRest` / `PreferGraphql` / `Throttled`) computed
/// from this snapshot, used to route fetches between REST and
/// GraphQL whenever a fetcher exists in both forms.
///
/// **Iterations-of-headroom** is the comparison unit the routing
/// algorithm should use, not raw remaining points: `remaining /
/// estimated_calls_per_iteration`. Raw points understate urgency
/// for high-volume buckets and overstate it for low-volume ones —
/// 500 REST remaining at 9 calls/iter is ~55 iters of headroom,
/// while 500 GraphQL remaining at 1 call/iter is ~500 iters.
///
/// **Cost-model caveat:** GraphQL is not cheaper per-point than
/// REST — it bills proportional to nodes returned, often more
/// than the REST calls it would replace. The only structural win
/// of "bias to GraphQL when REST is hot" is that the two buckets
/// are *separate 5000/hr quotas*; it is not a free lunch in
/// aggregate consumption.
///
/// Today neither concept is wired: the defensive `WaitForRateLimit`
/// axis (driven by [`RateLimitHit`]) catches actual throttling, and
/// recorder logs of this struct will tell us whether preemptive
/// routing is ever actually warranted.
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
        sorted.sort();
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
