//! Action shapes — the concrete operations decide can prescribe
//! for the codex-review domain.
//!
//! Each action carries:
//!   * `kind`: a typed enum variant naming the action and its payload
//!   * `automation`: who executes (us, an agent, a human, just wait)
//!   * `target_effect`: how this action changes batch/level state
//!   * `description`: human-readable prompt material for handoff
//!   * `urgency`: declared sort priority
//!   * `blocker`: stable iteration key for stall detection
//!
//! Domain notes:
//!   - `RunReviews` / `AwaitReviews` / `ParseVerdicts` — the
//!     observe-side procedural pipeline.
//!   - `AddressBatch` / `Retrospective` — Agent halts; outer
//!     orchestrator dispatches a Claude Task.
//!   - `AdvanceLevel` / `DropLevel` / `RestartFromFloor` — pure
//!     state transitions on the reasoning ladder.
//!   - `RunTests` — Full procedural test invocation.
//!   - `RequestCriteriaRefinement` — Human halt for ambiguous
//!     `--criteria`.

use std::time::Duration;

use serde::Serialize;

use crate::ids::BlockerKey;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action {
    pub kind: ActionKind,
    pub automation: Automation,
    pub target_effect: TargetEffect,
    pub urgency: Urgency,
    /// Human-readable. For agent handoff actions, this is the prompt.
    pub description: String,
    /// Stable iteration key — runner detects stalls by comparing
    /// (kind, blocker) against the prior iteration. The
    /// [`BlockerKey`] newtype prevents accidental confusion with
    /// `description` (also `String`-shaped) and documents the
    /// invariant that the value MUST NOT embed varying counts or
    /// other progress markers.
    pub blocker: BlockerKey,
}

/// Sort order for candidate actions. Lower variants are higher
/// priority. Codex-domain urgency: pipeline progress (Critical)
/// beats agent handoffs (BlockingFix) beats human handoffs
/// (BlockingHuman); polling waits sort below active work; tests
/// sort below state transitions; hygiene sorts last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Urgency {
    /// Pipeline-advancing Full actions: RunReviews,
    /// ParseVerdicts, AdvanceLevel, DropLevel, RestartFromFloor,
    /// RunTests. Free progress; never halt.
    Critical,
    /// Active Agent handoffs that fix or synthesize:
    /// AddressBatch, Retrospective.
    BlockingFix,
    /// Polling Wait: AwaitReviews. Loop sleeps and re-observes.
    BlockingWait,
    /// Human handoff: RequestCriteriaRefinement.
    BlockingHuman,
    /// Reserved for future advancing actions.
    Advancing,
    /// Reserved for future hygiene actions.
    Hygiene,
}

/// What dispatches the action. `Wait` carries the poll cadence so
/// "Wait without a sleep duration" is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Automation {
    /// We have the exact command and run it directly.
    Full,
    /// Hand off to an agent with the description as prompt.
    Agent,
    /// Wait for an external signal (codex subprocess completion)
    /// — poll after `interval` and re-iterate. `Duration` (not
    /// `u32`) so future backoff/jitter compose without changing
    /// the type.
    Wait {
        #[serde(skip)]
        interval: Duration,
    },
    /// Halt and surface to a human — only they can resolve.
    Human,
}

/// What dispatching this action would do to the batch/level state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TargetEffect {
    /// Action is the path past a current blocker.
    Blocks,
    /// Action moves the loop forward one step (most pipeline
    /// actions in this domain).
    Advances,
    /// Action is informational; no state-machine impact.
    Neutral,
}

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

    /// Halt for human input on ambiguous `--criteria`. Human
    /// automation. Reserved; not currently emitted by any code
    /// path. (Kept for symmetry with the planned `--criteria`
    /// disambiguation flow.)
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
