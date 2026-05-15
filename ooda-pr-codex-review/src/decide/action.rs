//! Action shapes — the concrete operations decide can prescribe.
//!
//! [`Action`], [`ActionEffect`], [`TargetEffect`], [`Urgency`] are
//! re-exported from [`ooda_core`] — the cross-binary spine. This
//! module owns the per-binary [`ActionKind`] enum (the PR domain's
//! action variants) and its [`ActionKindName`] impl. Payloads use
//! domain newtypes (`CheckName`, `GitHubLogin`) so a "right name in
//! the wrong position" bug is a compile error.

use crate::ids::{BlockerKey, CheckName, GitHubLogin, Reviewer};
use crate::orient::thread::ReviewThread;
pub use ooda_core::{ActionEffect, ActionKindName, NonEmpty, TargetEffect, Urgency};
use ooda_core::{RateLimitHit, RateLimitScope};
use serde::Serialize;

/// PR-domain `Action`. Concrete instantiation of the generic
/// [`ooda_core::Action`] over this binary's [`ActionKind`].
pub type Action = ooda_core::Action<ActionKind>;

/// Synthesize the action the runner executes when observe surfaces a
/// rate-limit hit. The action's effect is a [`ActionEffect::Wait`]
/// for the scope's `retry_after` — `act()` sleeps that duration and
/// the next iteration re-observes from fresh state. Urgency is
/// `Critical` because no other axis can produce useful work while
/// throttled; the blocker tag is the scope name so the recorder's
/// stall key separates rate-limit waits from other Waits.
pub fn rate_limit_wait_action(hit: RateLimitHit) -> Action {
    let log = format!(
        "rate-limited on {}; sleeping {}s",
        hit.scope.name(),
        hit.retry_after.as_duration().as_secs(),
    );
    Action {
        kind: ActionKind::WaitForRateLimit { scope: hit.scope },
        effect: ActionEffect::Wait {
            interval: hit.retry_after,
            log,
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Critical,
        blocker: BlockerKey::tag(hit.scope.name()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ActionKind {
    // ── CI ──
    FixCi {
        check_name: CheckName,
    },
    WaitForCi {
        pending: NonEmpty<CheckName>,
    },
    /// CI is blocked on a fan-in (e.g. Mergeability) AND something
    /// genuinely ambiguous is co-occurring (advisory failure). Hand
    /// to an agent to triage.
    TriageWait {
        blocked_checks: NonEmpty<CheckName>,
    },

    // ── Reviews ──
    /// Carries the live (non-resolved, non-outdated) review threads
    /// the actor must address. The full thread bodies travel with
    /// the action so the actor receives prompt material directly —
    /// no second `gh api graphql` round-trip to discover what to
    /// fix. `threads.len()` is the count; cardinality is a derived
    /// projection, not a stored field. (See feedback memory:
    /// "witness, not cardinality.")
    AddressThreads {
        threads: NonEmpty<ReviewThread>,
    },
    /// GitHub reports `CHANGES_REQUESTED` but no inline review threads
    /// exist (summary-only change request, or threads resolved without
    /// a re-approval). Distinct from `AddressThreads` because there is
    /// no thread payload to walk — the agent must read the latest
    /// `CHANGES_REQUESTED` review body and address the summary.
    AddressChangeRequest,
    RequestApproval,

    // ── Mechanical merge blockers ──
    Rebase,
    MarkReady,
    RemoveWipLabel,
    ShortenTitle {
        current_len: u32,
    },
    /// GitHub is still computing mergeability; observe again
    /// after a delay rather than halting Success on a transient
    /// post-push UNKNOWN.
    WaitForMergeability,
    /// `mergeStateStatus == BLOCKED` with no modeled axis
    /// explaining the blockage — typically an unmodeled merge
    /// policy (deployment protection, signed commits, custom
    /// ruleset). Hand off to a human; we don't know the gate.
    ResolveMergePolicy,

    // ── Metadata hygiene ──
    AddContentLabel,
    AddAssignee,
    AddDescription,

    // ── Bot tier advancement ──
    RerequestCopilot,
    WaitForCopilotAck,
    WaitForCopilotReview,
    AddressCopilotSuppressed {
        count: u32,
    },
    WaitForCursorReview,

    // ── Pending reviewers ──
    /// Bot reviewers always have logins (no `Team` variant).
    WaitForBotReview {
        reviewers: NonEmpty<GitHubLogin>,
    },
    /// Human reviewers may be a user OR a team — preserve the
    /// distinction at the type level.
    WaitForHumanReview {
        reviewers: NonEmpty<Reviewer>,
    },

    // ── Rate limits ──
    /// GitHub returned a rate-limit response from one of its quota
    /// buckets. The runner sleeps `retry_after` (carried on the
    /// `ActionEffect::Wait`) and re-observes from a clean state on
    /// the next iteration. Scope is preserved so the JSONL record
    /// and status comment identify which bucket fired.
    WaitForRateLimit {
        scope: RateLimitScope,
    },

    // ── Codex review axis ──
    /// Spawn `n` `codex review` subprocesses at the given reasoning
    /// level against the PR's current head. Full automation; the
    /// next iteration's observe sees the in-flight batch.
    RunCodexReviewBatch {
        level: crate::ids::CodexReasoningLevel,
        n: u32,
    },
    /// Codex review batch is streaming. Wait automation; runner
    /// sleeps then re-observes.
    AwaitCodexReviewBatch {
        level: crate::ids::CodexReasoningLevel,
        pending: u32,
    },
    /// Codex review batch completed with at least one reviewer
    /// flagging issues. Hand to an agent that verifies and addresses
    /// each issue. Description carries the verdict bodies as prompt.
    AddressCodexReviewBatch {
        level: crate::ids::CodexReasoningLevel,
        count: u32,
    },
}

impl ActionKind {
    /// The variant name only — the leading `Identifier` of the
    /// `Debug` form, with any payload (`{ ... }` or `(...)`)
    /// stripped. Used for the `<ActionKind>` placeholder in the
    /// SKILL.md stderr contract: caller-stable identity, no
    /// payload noise (which would expose internal data shapes
    /// and break the single-line invariant).
    pub fn name(&self) -> &'static str {
        ActionKindName::name(self)
    }
}

impl ActionKindName for ActionKind {
    fn name(&self) -> &'static str {
        match self {
            Self::FixCi { .. } => "FixCi",
            Self::WaitForCi { .. } => "WaitForCi",
            Self::TriageWait { .. } => "TriageWait",
            Self::AddressThreads { .. } => "AddressThreads",
            Self::AddressChangeRequest => "AddressChangeRequest",
            Self::RequestApproval => "RequestApproval",
            Self::Rebase => "Rebase",
            Self::MarkReady => "MarkReady",
            Self::RemoveWipLabel => "RemoveWipLabel",
            Self::ShortenTitle { .. } => "ShortenTitle",
            Self::WaitForMergeability => "WaitForMergeability",
            Self::ResolveMergePolicy => "ResolveMergePolicy",
            Self::AddContentLabel => "AddContentLabel",
            Self::AddAssignee => "AddAssignee",
            Self::AddDescription => "AddDescription",
            Self::RerequestCopilot => "RerequestCopilot",
            Self::WaitForCopilotAck => "WaitForCopilotAck",
            Self::WaitForCopilotReview => "WaitForCopilotReview",
            Self::AddressCopilotSuppressed { .. } => "AddressCopilotSuppressed",
            Self::WaitForCursorReview => "WaitForCursorReview",
            Self::WaitForBotReview { .. } => "WaitForBotReview",
            Self::WaitForHumanReview { .. } => "WaitForHumanReview",
            Self::WaitForRateLimit { .. } => "WaitForRateLimit",
            Self::RunCodexReviewBatch { .. } => "RunCodexReviewBatch",
            Self::AwaitCodexReviewBatch { .. } => "AwaitCodexReviewBatch",
            Self::AddressCodexReviewBatch { .. } => "AddressCodexReviewBatch",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooda_core::PollingInterval;

    /// Hand-maintained sample list of every [`RateLimitScope`]
    /// variant. The match in `rate_limit_action_round_trips_scope`
    /// is compile-checked exhaustive, so adding a new scope variant
    /// fails to compile until both this sample list and that match
    /// are updated.
    fn rate_limit_scope_samples() -> Vec<RateLimitScope> {
        vec![
            RateLimitScope::GitHubGraphqlPrimary,
            RateLimitScope::GitHubRestPrimary,
            RateLimitScope::GitHubSecondary,
        ]
    }

    #[test]
    fn rate_limit_action_round_trips_scope() {
        for scope in rate_limit_scope_samples() {
            let hit = RateLimitHit {
                scope,
                retry_after: PollingInterval::from_secs(60),
            };
            let action = rate_limit_wait_action(hit);
            // Compile-checked: every scope must route through this
            // match. Adding a variant breaks both this and ooda-core's
            // own exhaustive-match-as-contract test.
            match scope {
                RateLimitScope::GitHubGraphqlPrimary
                | RateLimitScope::GitHubRestPrimary
                | RateLimitScope::GitHubSecondary => {}
            }
            assert!(matches!(action.kind, ActionKind::WaitForRateLimit { .. }));
            assert!(matches!(action.effect, ActionEffect::Wait { .. }));
            assert_eq!(action.urgency, Urgency::Critical);
            assert_eq!(action.target_effect, TargetEffect::Blocks);
            // Blocker tag mirrors the scope so a primary-vs-secondary
            // rate-limit produces a distinct stall key.
            assert_eq!(action.blocker.as_str(), scope.name());
        }
    }

    #[test]
    fn rate_limit_action_log_mentions_scope_and_duration() {
        let hit = RateLimitHit {
            scope: RateLimitScope::GitHubGraphqlPrimary,
            retry_after: PollingInterval::from_secs(120),
        };
        let action = rate_limit_wait_action(hit);
        let ActionEffect::Wait { log, interval } = &action.effect else {
            panic!("expected Wait effect");
        };
        assert!(log.contains("github/graphql/primary"), "log: {log}");
        assert!(log.contains("120"), "log: {log}");
        assert_eq!(*interval, hit.retry_after);
    }
}
