//! Action shapes — the concrete operations decide can prescribe.
//!
//! Each action carries:
//!   * `kind`: a typed enum variant naming the action and its payload
//!   * `automation`: who executes (us, an agent, a human, just wait)
//!   * `target_effect`: how this action changes blocker/tier state
//!   * `description`: human-readable prompt material for handoff
//!
//! Payloads use domain newtypes (`CheckName`, `GitHubLogin`) rather
//! than `String` — promotes a class of "right name in the wrong
//! position" bugs into compile errors, and keeps each `Display`
//! impl on the type it describes.

use std::time::Duration;

use crate::ids::{BlockerKey, CheckName, GitHubLogin, Reviewer};
use crate::orient::thread::ReviewThread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub kind: ActionKind,
    pub automation: Automation,
    pub target_effect: TargetEffect,
    /// Declared sort priority. Replaces the prior tuple-comparator
    /// rule in `decide.rs` that derived priority from
    /// `(automation, target_effect)` — that comparator was changed
    /// 4 times across review passes 6, 7, 8, 12 because the
    /// judgment ("Full beats Blocks", "Blocks beats Hygiene", etc.)
    /// was emergent from the actions, not intrinsic to the
    /// automation enum. Each action now declares its urgency at
    /// construction; the sort is `urgency as u8` ascending.
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
/// priority. The split between BlockingFix/Wait/Human encodes the
/// "active fix beats passive handoff" rule directly in the enum
/// rather than a comparator tuple — adding a new urgency tier is a
/// single enum addition, no comparator change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// Full-automation actions. The loop runs them without halting,
    /// so they MUST preempt any blocking handoff — picking a Full
    /// action is free progress, while a blocking Wait/Human means
    /// the iteration ends with the PR still gated.
    Critical,
    /// Active fixes for blocking issues — Agent automation that
    /// addresses the blocker (AddressThreads, AddressChangeRequest,
    /// FixCi, Rebase, ShortenTitle, TriageWait).
    BlockingFix,
    /// Passive waits for blocking issues — Wait automation, the
    /// loop sleeps and re-observes (WaitForCi, WaitForCopilotAck,
    /// WaitForCopilotReview, WaitForHumanReview, WaitForBotReview,
    /// WaitForMergeability, WaitForCursorReview).
    BlockingWait,
    /// Human handoffs for blocking issues — only a human can
    /// resolve (RequestApproval, ResolveMergePolicy).
    BlockingHuman,
    /// Active advancement that doesn't unblock but improves the
    /// PR (AddressCopilotSuppressed). Non-Full Advances actions.
    Advancing,
    /// Non-blocking metadata cleanup (AddContentLabel, AddAssignee,
    /// AddDescription). Always sorts last regardless of automation.
    Hygiene,
}

/// What dispatches the action. `Wait` carries the poll cadence so
/// "Wait without a sleep duration" is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automation {
    /// We have the exact command and run it directly.
    Full,
    /// Hand off to an agent with the description as prompt.
    Agent,
    /// Wait for an external signal (CI, bot review, etc.) — poll
    /// after `interval` and re-iterate. `Duration` (not `u32`) so
    /// future backoff/jitter compose without changing the type.
    Wait { interval: Duration },
    /// Halt and surface to a human — only they can resolve.
    Human,
}

/// What dispatching this action would do to the blocker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetEffect {
    /// Action is the path past a current blocker.
    Blocks,
    /// Action moves the PR to a higher tier without unblocking.
    Advances,
    /// Action is informational; no blocker/tier impact.
    Neutral,
}

impl ActionKind {
    /// The variant name only — the leading `Identifier` of the
    /// `Debug` form, with any payload (`{ ... }` or `(...)`)
    /// stripped. Used for the `<ActionKind>` placeholder in the
    /// SKILL.md stderr contract: caller-stable identity, no
    /// payload noise (which would expose internal data shapes
    /// and break the single-line invariant).
    pub fn name(&self) -> &'static str {
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    // ── CI ──
    FixCi {
        check_name: CheckName,
    },
    WaitForCi {
        pending: Vec<CheckName>,
    },
    /// CI is blocked on a fan-in (e.g. Mergeability) AND something
    /// genuinely ambiguous is co-occurring (advisory failure). Hand
    /// to an agent to triage.
    TriageWait {
        blocked_checks: Vec<CheckName>,
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
        threads: Vec<ReviewThread>,
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
        reviewers: Vec<GitHubLogin>,
    },
    /// Human reviewers may be a user OR a team — preserve the
    /// distinction at the type level.
    WaitForHumanReview {
        reviewers: Vec<Reviewer>,
    },
}
