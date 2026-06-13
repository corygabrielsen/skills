//! Action shapes — the concrete operations decide can prescribe
//! for the codex-review domain.
//!
//! [`Action`], [`ActionEffect`], [`TargetEffect`], [`Urgency`] are
//! re-exports from [`ooda_core`] — the cross-binary spine. This
//! module owns the per-binary [`ActionKind`] enum, its
//! [`ActionKindName`] impl, and the [`CodexReasoningLevel`] ladder
//! type used in several action payloads.
//!
//! Domain partition:
//!
//! - **In-batch pipeline** — `RunReviews` / `AwaitReviews` /
//!   `ParseVerdicts` drive the observe-side state machine.
//! - **Agent handoffs** — `AddressBatch` / `Retrospective` /
//!   `TestsFailedTriage` exit the loop with a prompt for an
//!   external dispatcher.
//! - **Ladder transitions** — `AdvanceLevel` / `DropLevel` /
//!   `RestartFromFloor` mutate position on the reasoning ladder.
//! - **Test invocation** — `RunTests`.
//! - **Reserved** — `RequestCriteriaRefinement` is not yet
//!   emitted; declared so consumers can match exhaustively today.

pub(crate) use ooda_core::{ActionEffect, ActionKindName, TargetEffect, Urgency};
use serde::Serialize;

/// Codex-review-domain `Action`. Concrete instantiation of the
/// generic [`ooda_core::Action`] over this binary's [`ActionKind`].
pub(crate) type Action = ooda_core::Action<ActionKind>;

/// Reasoning effort level passed to `codex review` via
/// `-c model_reasoning_effort=<level>`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningLevel {
    Low,
    Medium,
    High,
    Xhigh,
}

impl CodexReasoningLevel {
    /// Canonical lowercase token used in CLI args, log file
    /// names, and recorder paths.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Next level up the ladder, or `None` at ceiling.
    pub fn higher(self) -> Option<Self> {
        match self {
            Self::Low => Some(Self::Medium),
            Self::Medium => Some(Self::High),
            Self::High => Some(Self::Xhigh),
            Self::Xhigh => None,
        }
    }

    /// Next level down the ladder, or `None` at floor.
    pub fn lower(self) -> Option<Self> {
        match self {
            Self::Xhigh => Some(Self::High),
            Self::High => Some(Self::Medium),
            Self::Medium => Some(Self::Low),
            Self::Low => None,
        }
    }
}

impl std::fmt::Display for CodexReasoningLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Closed enum whose `Display` returns a per-variant `&'static str`
// — gate-stable by construction; eligible for `GateIdentity`.
impl ooda_core::GateIdentity for CodexReasoningLevel {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ActionKind {
    /// Spawn `n` parallel `codex review` subprocesses at the
    /// given reasoning level. Full automation (returns immediately
    /// after spawn; `AwaitReviews` polls on subsequent iterations).
    RunReviews { level: CodexReasoningLevel, n: u32 },

    /// Poll the in-flight review subprocesses for completion.
    /// Wait automation — sleeps `interval`, then re-observes.
    AwaitReviews {
        level: CodexReasoningLevel,
        pending: u32,
    },

    /// Extract verdict blocks from completed log files, classify
    /// each (clean / has-issues), and merge issue records across
    /// the n reviews into a single batch. Full automation.
    ParseVerdicts { level: CodexReasoningLevel },

    /// Hand off the merged issue batch to Claude for verify-and-
    /// address. Agent automation; description is the prompt.
    AddressBatch {
        issue_count: u32,
        level: CodexReasoningLevel,
    },

    /// Hand off the issue history to Claude for retrospective
    /// pattern synthesis. Agent automation; runs after every
    /// per-level fixed point.
    Retrospective { level: CodexReasoningLevel },

    /// Climb one rung up the reasoning ladder (e.g., low →
    /// medium). Full automation; pure state transition.
    AdvanceLevel {
        from: CodexReasoningLevel,
        to: CodexReasoningLevel,
    },

    /// Drop one rung down after addressing issues, clamped at
    /// the configured floor. Full automation; pure state
    /// transition.
    DropLevel {
        from: CodexReasoningLevel,
        to: CodexReasoningLevel,
    },

    /// Reset to the configured floor level (after retrospective
    /// produced architectural changes that invalidate prior
    /// review fixed points). Full automation; pure state
    /// transition.
    RestartFromFloor { reason: String },

    /// Invoke the project's test suite (`make test` or
    /// equivalent). Full automation.
    RunTests,

    /// Halt for human input on ambiguous review criteria. Human
    /// automation. Reserved; not currently emitted by any code
    /// path.
    RequestCriteriaRefinement,

    /// Halt for human triage after the orchestrator reports tests
    /// failed (`--mark-address-failed`). The action's description
    /// embeds the orchestrator-supplied failure details. Human
    /// automation.
    TestsFailedTriage,

    /// Halt for human resolution when the observed batch state is
    /// inconsistent (e.g., more completed slots than expected — a
    /// stray log from a prior batch). The auto-loop has no policy
    /// for resolving stale state safely; surface to a human. Human
    /// automation.
    BatchStateInconsistent {
        level: CodexReasoningLevel,
        reason: String,
    },
}

impl ActionKind {
    /// Payload-free variant identifier. Stable token, no payload
    /// noise; used wherever caller-stable identity must appear
    /// without exposing internals.
    pub fn name(&self) -> &'static str {
        ActionKindName::name(self)
    }
}

impl ActionKindName for ActionKind {
    fn name(&self) -> &'static str {
        match self {
            Self::RunReviews { .. } => "RunReviews",
            Self::AwaitReviews { .. } => "AwaitReviews",
            Self::ParseVerdicts { .. } => "ParseVerdicts",
            Self::AddressBatch { .. } => "AddressBatch",
            Self::Retrospective { .. } => "Retrospective",
            Self::AdvanceLevel { .. } => "AdvanceLevel",
            Self::DropLevel { .. } => "DropLevel",
            Self::RestartFromFloor { .. } => "RestartFromFloor",
            Self::RunTests => "RunTests",
            Self::RequestCriteriaRefinement => "RequestCriteriaRefinement",
            Self::TestsFailedTriage => "TestsFailedTriage",
            Self::BatchStateInconsistent { .. } => "BatchStateInconsistent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn higher_climbs_one_rung() {
        assert_eq!(
            CodexReasoningLevel::Low.higher(),
            Some(CodexReasoningLevel::Medium),
        );
        assert_eq!(
            CodexReasoningLevel::Medium.higher(),
            Some(CodexReasoningLevel::High),
        );
        assert_eq!(
            CodexReasoningLevel::High.higher(),
            Some(CodexReasoningLevel::Xhigh),
        );
    }

    #[test]
    fn higher_at_ceiling_yields_none() {
        assert_eq!(CodexReasoningLevel::Xhigh.higher(), None);
    }

    #[test]
    fn lower_drops_one_rung() {
        assert_eq!(
            CodexReasoningLevel::Xhigh.lower(),
            Some(CodexReasoningLevel::High),
        );
        assert_eq!(
            CodexReasoningLevel::High.lower(),
            Some(CodexReasoningLevel::Medium),
        );
        assert_eq!(
            CodexReasoningLevel::Medium.lower(),
            Some(CodexReasoningLevel::Low),
        );
    }

    #[test]
    fn lower_at_floor_yields_none() {
        assert_eq!(CodexReasoningLevel::Low.lower(), None);
    }
}
