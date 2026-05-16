//! Typed view of `GET /rate_limit`.
//!
//! The `/rate_limit` endpoint is **free** — it does not count against
//! quota — and returns every bucket's current counters in one
//! response. The loop calls it once per iteration so the recorder
//! has a per-iteration snapshot of `{ core, graphql }.{ remaining,
//! limit, reset_at }`. Today nothing acts on the snapshot beyond
//! recorder logging; see [`ooda_core::RateLimitBudget`]'s doc for
//! the named-but-unimplemented routing concepts (`BucketBias`,
//! iterations-of-headroom).
//!
//! GitHub's wire shape names the field `reset` (unix epoch seconds);
//! [`ooda_core::BucketState`] uses `reset_at_epoch` internally for
//! clarity. The local wire types here do the rename so the boundary
//! mapping is explicit and confined to one function.

use ooda_core::{BucketState, RateLimitBudget};
use serde::Deserialize;

use super::gh::{GhError, gh_json};

/// Fetch the current rate-limit snapshot. Calling this does NOT
/// consume quota. Returns the two buckets the OODA family uses
/// (REST primary + GraphQL primary); secondary-limit counters and
/// the legacy top-level `rate` field are ignored.
pub(crate) fn fetch_rate_limit_budget() -> Result<RateLimitBudget, GhError> {
    let wire: RateLimitWire = gh_json(&["api", "rate_limit"])?;
    Ok(RateLimitBudget {
        rest: wire.resources.core.into(),
        graphql: wire.resources.graphql.into(),
    })
}

#[derive(Debug, Deserialize)]
struct RateLimitWire {
    resources: Resources,
}

#[derive(Debug, Deserialize)]
struct Resources {
    core: BucketWire,
    graphql: BucketWire,
}

#[derive(Debug, Deserialize)]
struct BucketWire {
    limit: u32,
    remaining: u32,
    reset: u64,
}

impl From<BucketWire> for BucketState {
    fn from(w: BucketWire) -> Self {
        Self {
            remaining: w.remaining,
            limit: w.limit,
            reset_at_epoch: w.reset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the boundary mapping: a realistic `/rate_limit` response
    /// payload (subset — extra fields like `search` and the legacy
    /// top-level `rate` are ignored by serde's default) must
    /// deserialize and project cleanly into [`RateLimitBudget`].
    #[test]
    fn projects_github_wire_payload() {
        let body = r#"{
          "resources": {
            "core":   {"limit":5000,"remaining":4999,"reset":1372700873,"used":1},
            "graphql":{"limit":5000,"remaining":4500,"reset":1372700900,"used":500},
            "search": {"limit":30,  "remaining":30,  "reset":1372700873,"used":0}
          },
          "rate":     {"limit":5000,"remaining":4999,"reset":1372700873,"used":1}
        }"#;
        let wire: RateLimitWire = serde_json::from_str(body).unwrap();
        let budget = RateLimitBudget {
            rest: wire.resources.core.into(),
            graphql: wire.resources.graphql.into(),
        };
        assert_eq!(budget.rest.remaining, 4999);
        assert_eq!(budget.rest.limit, 5000);
        assert_eq!(budget.rest.reset_at_epoch, 1_372_700_873);
        assert_eq!(budget.graphql.remaining, 4500);
        assert_eq!(budget.graphql.limit, 5000);
        assert_eq!(budget.graphql.reset_at_epoch, 1_372_700_900);
    }
}
