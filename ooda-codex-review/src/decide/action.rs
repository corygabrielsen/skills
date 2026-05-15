//! Action shapes — the concrete operations decide can prescribe
//! for the codex-review domain.
//!
//! [`Action`], [`ActionEffect`], [`TargetEffect`], [`Urgency`] are
//! re-exported from [`ooda_core`] — the cross-binary spine. This
//! module owns the per-binary [`ActionKind`] enum (the codex-review
//! domain's action variants), its [`ActionKindName`] impl, and the
//! [`ReasoningLevel`] ladder type referenced by several action
//! payloads.
//!
//! Domain notes:
//!   - `RunReviews` / `AwaitReviews` / `ParseVerdicts` — the
//!     observe-side procedural pipeline.
//!   - `AddressBatch` / `Retrospective` — Agent halts; outer
//!     orchestrator dispatches a Claude Task.
//!   - `AdvanceLevel` / `DropLevel` / `RestartFromFloor` — pure
//!     state transitions on the reasoning ladder.
//!   - `RunTests` — Full procedural test invocation.
//!   - `RequestCriteriaRefinement` — reserved human halt for any
//!     future review-criteria flow.

pub use ooda_core::{ActionEffect, ActionKindName, TargetEffect, Urgency};
use serde::Serialize;

/// Codex-review-domain `Action`. Concrete instantiation of the
/// generic [`ooda_core::Action`] over this binary's [`ActionKind`].
pub type Action = ooda_core::Action<ActionKind>;

/// Reasoning effort level passed to `codex review` via
/// `-c model_reasoning_effort=<level>`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningLevel {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ActionKind {
    /// Spawn `n` parallel `codex review` subprocesses at the
    /// given reasoning level. Full automation (returns immediately
    /// after spawn; AwaitReviews polls on subsequent iterations).
    RunReviews { level: ReasoningLevel, n: u32 },

    /// Poll the in-flight review subprocesses for completion.
    /// Wait automation — sleeps `interval`, then re-observes.
    AwaitReviews { level: ReasoningLevel, pending: u32 },

    /// Extract verdict blocks from completed log files, classify
    /// each (clean / has-issues), and merge issue records across
    /// the n reviews into a single batch. Full automation.
    ParseVerdicts { level: ReasoningLevel },

    /// Hand off the merged issue batch to Claude for verify-and-
    /// address. Agent automation; description is the prompt.
    AddressBatch {
        issue_count: u32,
        level: ReasoningLevel,
    },

    /// Hand off the issue history to Claude for retrospective
    /// pattern synthesis. Agent automation; runs after every
    /// per-level fixed point.
    Retrospective { level: ReasoningLevel },

    /// Climb one rung up the reasoning ladder (e.g., low →
    /// medium). Full automation; pure state transition.
    AdvanceLevel {
        from: ReasoningLevel,
        to: ReasoningLevel,
    },

    /// Drop one rung down after addressing issues, clamped at
    /// the configured floor. Full automation; pure state
    /// transition.
    DropLevel {
        from: ReasoningLevel,
        to: ReasoningLevel,
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
}

impl ActionKind {
    /// The variant name only — the leading `Identifier` of the
    /// `Debug` form, with any payload (`{ ... }` or `(...)`)
    /// stripped. Used for the `<ActionKind>` placeholder in the
    /// SKILL.md stderr contract: caller-stable identity, no
    /// payload noise.
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
        }
    }
}
