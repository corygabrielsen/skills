//! Action shapes — the concrete operations decide can prescribe.
//!
//! [`Action`], [`ActionEffect`], [`TargetEffect`], [`Urgency`] are
//! re-exported from [`ooda_core`] — the cross-binary spine. This
//! module owns the per-binary [`ActionKind`] enum (the PR domain's
//! action variants) and its [`ActionKindName`] impl. Payloads use
//! domain newtypes (`CheckName`, `GitHubLogin`) so a "right name in
//! the wrong position" bug is a compile error.

use crate::ids::{BlockerKey, CheckName, GitHubLogin, Reviewer};
use crate::observe::github::workflow_runs::WorkflowRunId;
use crate::orient::ci::Symptom as CiSymptom;
use crate::orient::copilot::Symptom;
use crate::orient::thread::ReviewThread;
pub(crate) use ooda_core::{ActionEffect, ActionKindName, NonEmpty, TargetEffect, Urgency};
use ooda_core::{RateLimitHit, RateLimitScope};
use serde::Serialize;

/// PR-domain `Action`. Concrete instantiation of the generic
/// [`ooda_core::Action`] over this binary's [`ActionKind`].
pub(crate) type Action = ooda_core::Action<ActionKind>;

/// Payload for [`ActionKind::ReRunWorkflow`]: one degraded check
/// with its workflow run handle (consumed by the act layer to issue
/// the rerun) and the triggering symptom (recorded in the blocker
/// tag so the stall comparator separates queue-timeout from
/// run-timeout stalls).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DegradedCheck {
    pub name: CheckName,
    pub run_id: WorkflowRunId,
    pub symptom: CiSymptom,
}

/// Payload for [`ActionKind::EscalateCiFailed`]: one Failed check
/// with the triggering symptom. No workflow run handle — escalation
/// has no side effect, only naming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedCheckHandle {
    pub name: CheckName,
    pub symptom: CiSymptom,
}

/// Synthesize the action the runner executes when observe surfaces a
/// rate-limit hit. The action's effect is a [`ActionEffect::Wait`]
/// for the scope's `retry_after` — `act()` sleeps that duration and
/// the next iteration re-observes from fresh state. Urgency is
/// `Critical` because no other axis can produce useful work while
/// throttled; the blocker tag is the scope name so the recorder's
/// stall key separates rate-limit waits from other Waits.
pub(crate) fn rate_limit_wait_action(hit: RateLimitHit) -> Action {
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
        blocker: BlockerKey::from_static(hit.scope.name()),
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
    /// Health-driven remediation. One or more required checks crossed
    /// a queue/run timeout on the current HEAD and re-run budget is
    /// not yet exhausted. The act layer issues `POST
    /// /repos/:o/:r/actions/runs/:run_id/rerun` for each entry; the
    /// next iteration sees a fresh workflow run as Healthy.
    ReRunWorkflow {
        checks: NonEmpty<DegradedCheck>,
    },
    /// Per-(check, HEAD) re-run budget exhausted on at least one
    /// required check; humans must triage. No automatic side effect
    /// — decide hands off via `ActionEffect::Human`. The action
    /// payload carries every Failed check so the prompt names them.
    EscalateCiFailed {
        checks: NonEmpty<FailedCheckHandle>,
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
    // Degraded-axis remediation. CI's ReRunWorkflow +
    // EscalateCiFailed (above) wear the same Healthy/Degraded/Failed
    // shape; on the 3rd axis lift to ooda_core::AxisHealth<S>.
    RerequestCopilot {
        /// Health-driven remediation carries the triggering symptom;
        /// tier-advancement re-requests (no health degradation) pass
        /// `None`. The variant is the same — the side effect is
        /// identical (POST `requested_reviewers`).
        symptom: Option<Symptom>,
    },
    /// Per-HEAD health budget exhausted; humans must triage. No
    /// automatic side effect — decide hands off via
    /// `ActionEffect::Human`.
    EscalateCopilotFailed {
        symptom: Symptom,
    },
    WaitForCopilotAck,
    WaitForCopilotReview,
    AddressCopilotSuppressed {
        count: u32,
    },
    WaitForCursorReview,
    /// Cursor's `check_suite` is stalled past `STALL_TIMEOUT` on Cursor's
    /// backend and there is no remediation API (posting a `cursor
    /// review` comment does not unstick Cursor's own queue). Hand off
    /// to a human; no side effect — decide emits this via
    /// `ActionEffect::Human`, the runner translates to
    /// `Outcome::HandoffHuman`, and the act layer never sees it.
    /// Deliberately payload-free: Cursor has a single failure mode
    /// (stalled suite), unlike Copilot's StartTimeout/ReviewTimeout
    /// or CI's QueueTimeout/RunTimeout, so there is no Symptom to
    /// carry.
    EscalateCursorStalled,

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

    // ── PR metadata attestation ──
    /// PR title / description / labels are out of sync with HEAD
    /// (Drift) or have never been attested for this PR
    /// (`NeverAttested`). Hand off to an agent to refresh PR meta and
    /// re-run `ooda-attest pr-meta` to write a fresh attestation.
    /// Payload carries the absolute path of the attestation file so
    /// the prompt can surface the exact CLI invocation.
    SyncPullRequestMetadata {
        attest_path: std::path::PathBuf,
    },

    // ── Doc review attestation ──
    /// Doc / comment hygiene attestation is out of sync with HEAD
    /// (Drift) or has never been recorded (`NeverAttested`). Hand off
    /// to an agent to review the full PR diff and re-run
    /// `ooda-attest doc-review`.
    ReviewDocs {
        attest_path: std::path::PathBuf,
    },

    // ── Claude review attestation ──
    /// Claude has posted review content past the last attestation
    /// (`Fresh`). Hand off to an agent to address the threads + body
    /// and re-run `ooda-attest claude-review`. Distinct from the
    /// SHA-based attestation axes: the trigger is *content* drift,
    /// not HEAD-SHA drift.
    AddressClaudeReview {
        attest_path: std::path::PathBuf,
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
            Self::ReRunWorkflow { .. } => "ReRunWorkflow",
            Self::EscalateCiFailed { .. } => "EscalateCiFailed",
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
            Self::RerequestCopilot { .. } => "RerequestCopilot",
            Self::EscalateCopilotFailed { .. } => "EscalateCopilotFailed",
            Self::WaitForCopilotAck => "WaitForCopilotAck",
            Self::WaitForCopilotReview => "WaitForCopilotReview",
            Self::AddressCopilotSuppressed { .. } => "AddressCopilotSuppressed",
            Self::WaitForCursorReview => "WaitForCursorReview",
            Self::EscalateCursorStalled => "EscalateCursorStalled",
            Self::WaitForBotReview { .. } => "WaitForBotReview",
            Self::WaitForHumanReview { .. } => "WaitForHumanReview",
            Self::WaitForRateLimit { .. } => "WaitForRateLimit",
            Self::SyncPullRequestMetadata { .. } => "SyncPullRequestMetadata",
            Self::ReviewDocs { .. } => "ReviewDocs",
            Self::AddressClaudeReview { .. } => "AddressClaudeReview",
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
