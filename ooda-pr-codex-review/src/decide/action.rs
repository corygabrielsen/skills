//! Action shapes — the concrete operations decide can prescribe.
//!
//! The generic action carrier and its effect / target / urgency
//! shapes live in the shared crate. This module fixes the type
//! parameter to the PR-domain action variant and supplies the
//! discriminant projection. Payloads carry domain newtypes so a
//! type-correct call cannot mix identifiers from different
//! namespaces.

use crate::ids::{BlockerKey, CheckName, GitHubLogin, Reviewer};
use crate::observe::github::workflow_runs::WorkflowRunId;
use crate::orient::ci::Symptom as CiSymptom;
use crate::orient::copilot::Symptom;
use crate::orient::thread::ReviewThread;
pub(crate) use ooda_core::{ActionEffect, ActionKindName, NonEmpty, TargetEffect, Urgency};
use ooda_core::{RateLimitHit, RateLimitScope};
use serde::Serialize;

/// PR-domain action specialised to this binary's discriminant.
pub(crate) type Action = ooda_core::Action<ActionKind>;

/// Per-check payload for the workflow-rerun action: identifier, the
/// upstream run handle the act layer needs to issue the rerun, and
/// the symptom carried into the blocker key so distinct timeout
/// classes do not collide in stall detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DegradedCheck {
    pub name: CheckName,
    pub run_id: WorkflowRunId,
    pub symptom: CiSymptom,
}

/// Per-check payload for the failed-CI escalation. No run handle —
/// the escalation has no driver-side effect; it names what needs
/// human attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedCheckHandle {
    pub name: CheckName,
    pub symptom: CiSymptom,
}

/// Synthesize the action for an observed rate-limit hit. The effect
/// is a Wait for the upstream's retry window; urgency is the most
/// critical tier because no other axis can produce useful work while
/// throttled. The blocker key is the scope identifier so distinct
/// upstream quota buckets do not collide in stall detection.
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
    /// CI is blocked on a fan-in AND an ambiguous advisory failure
    /// co-occurs. The combined signal needs agent triage.
    TriageWait {
        blocked_checks: NonEmpty<CheckName>,
    },
    /// Driver-side remediation: required checks crossed a timeout
    /// at HEAD with per-check rerun budget remaining. The act layer
    /// issues the rerun and the next iteration re-observes.
    ReRunWorkflow {
        checks: NonEmpty<DegradedCheck>,
    },
    /// Per-(check, HEAD) rerun budget exhausted. Decide hands off
    /// to a human; the payload names every failed check so the
    /// prompt can list them.
    EscalateCiFailed {
        checks: NonEmpty<FailedCheckHandle>,
    },

    // ── Reviews ──
    /// Carries the unresolved review threads the actor must address.
    /// Payload-as-witness: the full thread bodies travel inline, so
    /// the actor consumes one prompt instead of round-tripping to
    /// the upstream review API. Count is derived from the payload,
    /// never stored separately.
    AddressThreads {
        threads: NonEmpty<ReviewThread>,
    },
    /// Summary-only change request: the upstream reports
    /// changes-requested but no inline threads exist. Distinct from
    /// the per-thread case because the only feedback to address is
    /// the latest review body.
    AddressChangeRequest,
    RequestApproval,

    // ── Mechanical merge blockers ──
    Rebase,
    MarkReady,
    RemoveWipLabel,
    ShortenTitle {
        current_len: u32,
    },
    /// Upstream mergeability is still computing; wait and
    /// re-observe rather than halting on a transient unknown.
    WaitForMergeability,
    /// Upstream reports merge-blocked but no modeled axis
    /// explains the gate — an unmodeled merge policy is in play.
    /// Human triage owns it; the gate is not known to the driver.
    ResolveMergePolicy,

    // ── Metadata hygiene ──
    AddContentLabel,
    AddAssignee,
    AddDescription,

    // ── Bot tier advancement ──
    // Bot-axis remediation wears the same Healthy/Degraded/Failed
    // shape CI uses; a future third axis can lift the common form.
    RerequestCopilot {
        /// `Some(symptom)` when the re-request is health-driven;
        /// `None` for tier-advancement re-requests on a healthy
        /// axis. The side effect is the same — only the blocker
        /// key separates the cases.
        symptom: Option<Symptom>,
    },
    /// Per-HEAD health budget exhausted. Hand off to a human; no
    /// driver-side side effect.
    EscalateCopilotFailed {
        symptom: Symptom,
    },
    WaitForCopilotAck,
    WaitForCopilotReview,
    AddressCopilotSuppressed {
        count: u32,
    },
    WaitForCursorReview,
    /// Cursor's review surface is stalled past the upstream timeout
    /// with no remediation API. Hand off to a human. Payload-free:
    /// this axis has a single failure mode, so there is no symptom
    /// discriminator to carry.
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
    /// Upstream throttled the observe stage. The driver sleeps
    /// the retry window carried on the Wait effect and re-observes
    /// on the next iteration. Scope is preserved so distinct
    /// upstream quota buckets remain distinguishable in records.
    WaitForRateLimit {
        scope: RateLimitScope,
    },

    // ── Codex review axis ──
    /// Driver-side launch: spawn a batch of codex-review subprocesses
    /// at the named reasoning level against current HEAD. The next
    /// iteration's observe surfaces the in-flight batch.
    RunCodexReviewBatch {
        level: crate::ids::CodexReasoningLevel,
        n: u32,
    },
    /// Batch is in-flight at this reasoning level. Wait and
    /// re-observe; the payload count is the cardinality of pending
    /// reviewers, carried so the dedup key flips as reviewers land.
    AwaitCodexReviewBatch {
        level: crate::ids::CodexReasoningLevel,
        pending: u32,
    },
    /// Batch completed with at least one reviewer flagging issues
    /// at this reasoning level. Agent handoff: the action prompt
    /// carries the verdict bodies so the agent has the material
    /// inline. Count is a derived projection over the payload.
    AddressCodexReviewBatch {
        level: crate::ids::CodexReasoningLevel,
        count: u32,
    },

    // ── PR metadata attestation ──
    /// PR metadata attestation drifted from HEAD or has never been
    /// recorded. Agent handoff to refresh metadata and re-attest;
    /// payload carries the attestation file path so the prompt
    /// can surface the exact CLI invocation.
    SyncPullRequestMetadata {
        attest_path: std::path::PathBuf,
    },

    // ── Doc review attestation ──
    /// Doc / comment hygiene attestation drifted from HEAD or has
    /// never been recorded. Agent handoff to review the diff and
    /// re-attest.
    ReviewDocs {
        attest_path: std::path::PathBuf,
    },

    // ── Claude review attestation ──
    /// Content-keyed attestation: review content has advanced past
    /// the last attestation. The trigger is content drift rather
    /// than HEAD drift.
    AddressClaudeReview {
        attest_path: std::path::PathBuf,
    },

    // ── Closeout attestation ──
    /// Convergence-gate attestation. Emitted at the least-urgent
    /// tier so it wins only on global quiescence; the agent
    /// performs a pre-handoff sweep and re-attests at HEAD before
    /// the terminal human handoff fires.
    Closeout {
        attest_path: std::path::PathBuf,
    },
}

impl ActionKind {
    /// Payload-free discriminant — caller-stable identity for
    /// surfaces that must be single-line and decoupled from
    /// internal payload shape (logs, headers, dedup keys).
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
            Self::RunCodexReviewBatch { .. } => "RunCodexReviewBatch",
            Self::AwaitCodexReviewBatch { .. } => "AwaitCodexReviewBatch",
            Self::AddressCodexReviewBatch { .. } => "AddressCodexReviewBatch",
            Self::SyncPullRequestMetadata { .. } => "SyncPullRequestMetadata",
            Self::ReviewDocs { .. } => "ReviewDocs",
            Self::AddressClaudeReview { .. } => "AddressClaudeReview",
            Self::Closeout { .. } => "Closeout",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooda_core::PollingInterval;

    /// Hand-maintained sample list — one entry per scope variant.
    /// Paired with a compile-checked exhaustive match in the
    /// round-trip test below, so a new variant fails to compile
    /// until both are updated.
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
            // Compile-checked exhaustive over scope variants.
            match scope {
                RateLimitScope::GitHubGraphqlPrimary
                | RateLimitScope::GitHubRestPrimary
                | RateLimitScope::GitHubSecondary => {}
            }
            assert!(matches!(action.kind, ActionKind::WaitForRateLimit { .. }));
            assert!(matches!(action.effect, ActionEffect::Wait { .. }));
            assert_eq!(action.urgency, Urgency::Critical);
            assert_eq!(action.target_effect, TargetEffect::Blocks);
            // Blocker carries scope identity ⇒ distinct upstream
            // buckets project to distinct stall keys.
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
