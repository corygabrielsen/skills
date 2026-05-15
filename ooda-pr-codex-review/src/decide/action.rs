//! Action shapes — the concrete operations decide can prescribe.
//!
//! [`Action`], [`Automation`], [`TargetEffect`], [`Urgency`] are
//! re-exported from [`ooda_core`] — the cross-binary spine. This
//! module owns the per-binary [`ActionKind`] enum (the PR domain's
//! action variants) and its [`ActionKindName`] impl. Payloads use
//! domain newtypes (`CheckName`, `GitHubLogin`) so a "right name in
//! the wrong position" bug is a compile error.

use crate::ids::{CheckName, GitHubLogin, Reviewer};
use crate::orient::thread::ReviewThread;
pub use ooda_core::{ActionKindName, Automation, NonEmpty, TargetEffect, Urgency};
use serde::Serialize;

/// PR-domain `Action`. Concrete instantiation of the generic
/// [`ooda_core::Action`] over this binary's [`ActionKind`].
pub type Action = ooda_core::Action<ActionKind>;

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

    // ── Codex review axis ──
    /// Spawn `n` `codex review` subprocesses at the given reasoning
    /// level against the PR's current head. Full automation; the
    /// next iteration's observe sees the in-flight batch.
    RunCodexReviewBatch {
        level: crate::ids::ReasoningLevel,
        n: u32,
    },
    /// Codex review batch is streaming. Wait automation; runner
    /// sleeps then re-observes.
    AwaitCodexReviewBatch {
        level: crate::ids::ReasoningLevel,
        pending: u32,
    },
    /// Codex review batch completed with at least one reviewer
    /// flagging issues. Hand to an agent that verifies and addresses
    /// each issue. Description carries the verdict bodies as prompt.
    AddressCodexReviewBatch {
        level: crate::ids::ReasoningLevel,
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
            Self::RunCodexReviewBatch { .. } => "RunCodexReviewBatch",
            Self::AwaitCodexReviewBatch { .. } => "AwaitCodexReviewBatch",
            Self::AddressCodexReviewBatch { .. } => "AddressCodexReviewBatch",
        }
    }
}
