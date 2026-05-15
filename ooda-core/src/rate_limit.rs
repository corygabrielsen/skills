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
}
