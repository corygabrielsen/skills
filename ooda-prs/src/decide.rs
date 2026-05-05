//! Decide stage: take an OrientedState, generate candidate actions
//! per axis, rank, and emit a Decision (Execute or Halt).
//!
//! Per the design conversation: halt is a *predicate over the
//! candidate set*, not a scalar comparison. There is no
//! `score >= target` check here — score is display-only. We halt
//! when the candidate set is empty (Success) or when the top
//! candidate requires an external resolver (Agent / Human).

pub mod action;
mod ci;
mod copilot;
mod cursor;
pub mod decision;
mod reviews;
mod state;

use crate::observe::github::pr_view::PrState;
use crate::orient::OrientedState;

use action::{Action, Automation, TargetEffect};
use decision::{Decision, DecisionHalt, Terminal};

// `Urgency` is referenced only by tests in this module; suppress the
// unused-import lint by keeping the use behind cfg(test).
#[cfg(test)]
use action::Urgency;

/// Decide the next action for a PR given its oriented state.
///
/// Lifecycle terminal cases short-circuit: merged or closed PRs
/// have no advancement available.
///
/// Otherwise, generate candidates from each axis, take the first,
/// classify by automation:
///   * Full   → Execute (the loop runs the action itself)
///   * Wait   → Execute (the loop sleeps and re-observes)
///   * Agent  → Halt(AgentNeeded(action))
///   * Human  → Halt(HumanNeeded(action))
///
/// Empty candidate set → Halt(Success).
pub fn decide(oriented: &OrientedState, lifecycle: PrState) -> Decision {
    decide_from_candidates(candidates(oriented), lifecycle)
}

pub(crate) fn decide_from_candidates(candidates: Vec<Action>, lifecycle: PrState) -> Decision {
    match lifecycle {
        PrState::Merged => return Decision::Halt(DecisionHalt::Terminal(Terminal::Merged)),
        PrState::Closed => return Decision::Halt(DecisionHalt::Terminal(Terminal::Closed)),
        PrState::Open => {}
    }

    let Some(top) = candidates.into_iter().next() else {
        return Decision::Halt(DecisionHalt::Success);
    };

    classify(top)
}

fn classify(action: Action) -> Decision {
    match action.automation {
        Automation::Full | Automation::Wait { .. } => Decision::Execute(action),
        Automation::Agent => Decision::Halt(DecisionHalt::AgentNeeded(action)),
        Automation::Human => Decision::Halt(DecisionHalt::HumanNeeded(action)),
    }
}

/// Generate ranked candidate actions across all axes.
///
/// Two-phase ranking:
/// 1. Collect by axis — mechanical state blockers, CI, reviews,
///    Copilot, Cursor, hygiene. Within each axis, the per-axis
///    `candidates()` function decides its own ordering.
/// 2. Stable-sort by automation priority so that any active
///    fix-it-now candidate beats any passive wait/handoff,
///    regardless of axis order. This is the class invariant:
///    a candidate the system can drive (Full/Agent) MUST preempt
///    a candidate that just waits for an external signal
///    (Wait/Human). Without this rule, e.g. a `WaitForHumanReview`
///    from the reviews axis would beat a `RerequestCopilot` from
///    the copilot axis, leaving free advancement on the table.
///    Stable sort preserves axis order within each priority
///    bucket, so within "all Agent actions" the existing axis
///    rationale (state-before-ci-before-reviews) still applies.
pub(crate) fn candidates(oriented: &OrientedState) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    // Mechanical state blockers (rebase, mark_ready, remove_wip,
    // shorten_title) come before CI: a draft PR's required checks
    // won't even start until it's marked ready, and CI failures on
    // a conflicted/behind branch are noise until the merge base is
    // resolved.
    out.extend(state::blocking_candidates(&oriented.state));
    out.extend(ci::candidates(&oriented.ci));
    out.extend(reviews::candidates(oriented));
    if let Some(c) = &oriented.copilot {
        out.extend(copilot::candidates(c));
    }
    if let Some(c) = &oriented.cursor {
        out.extend(cursor::candidates(c));
    }
    // Fallback merge-state blocker: only fires when NO axis can
    // already advance or unblock the PR. Catches unmodeled policy
    // gates (deployment protection, signed commits, custom
    // rulesets) that would otherwise let decide() halt Success on
    // a still-unmergeable PR.
    //
    // Suppress on either Blocks or Advances — an Advances candidate
    // (e.g. RerequestCopilot when Copilot is the gate) means there
    // IS a path to merge; the Human ResolveMergePolicy handoff
    // would shadow the actionable Full/Agent advancement otherwise.
    // Hygiene-only (Neutral) does NOT suppress: hygiene doesn't
    // unblock BLOCKED, so the fallback Human handoff is still
    // correct.
    let has_advancement_path = out.iter().any(|a| {
        matches!(
            a.target_effect,
            TargetEffect::Blocks | TargetEffect::Advances,
        )
    });
    if !has_advancement_path {
        out.extend(state::fallback_merge_state_blocker(&oriented.state));
    }
    // Hygiene candidates (AddContentLabel, AddAssignee,
    // AddDescription) are intentionally NOT emitted: they encode
    // project-specific conventions (label vocabulary, description
    // structure) that don't apply to most repos. Halting the loop
    // on those would prevent convergence on otherwise-clean PRs.
    // The state fields are still computed and surfaced in the PR
    // comment renderer for visibility — they just don't drive the
    // decide stage.
    out.sort_by_key(|a| a.urgency);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::BlockerKey;
    use action::{ActionKind, TargetEffect};

    fn act(name: &str, urgency: Urgency) -> Action {
        Action {
            kind: ActionKind::RequestApproval,
            automation: Automation::Human,
            target_effect: TargetEffect::Blocks,
            urgency,
            description: name.into(),
            blocker: BlockerKey::tag(name),
        }
    }

    #[test]
    fn urgency_total_order_matches_design_intent() {
        // The enum is the rule: Critical < BlockingFix < BlockingWait
        // < BlockingHuman < Advancing < Hygiene. Any future tier slots
        // into the enum at the right position; the sort doesn't change.
        assert!(Urgency::Critical < Urgency::BlockingFix);
        assert!(Urgency::BlockingFix < Urgency::BlockingWait);
        assert!(Urgency::BlockingWait < Urgency::BlockingHuman);
        assert!(Urgency::BlockingHuman < Urgency::Advancing);
        assert!(Urgency::Advancing < Urgency::Hygiene);
    }

    #[test]
    fn priority_sort_is_stable_within_urgency() {
        // Two BlockingFix actions in the same urgency tier must
        // remain in axis order after the sort.
        let mut v = [
            act("fix-a", Urgency::BlockingFix),
            act("wait", Urgency::BlockingWait),
            act("fix-b", Urgency::BlockingFix),
            act("critical", Urgency::Critical),
            act("human", Urgency::BlockingHuman),
            act("hygiene", Urgency::Hygiene),
        ];
        v.sort_by_key(|a| a.urgency);
        let order: Vec<&str> = v.iter().map(|a| a.blocker.as_str()).collect();
        assert_eq!(
            order,
            vec!["critical", "fix-a", "fix-b", "wait", "human", "hygiene"]
        );
    }
}
