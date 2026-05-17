//! Decide stage: per-axis candidate generation, rank, and emit a
//! Decision.
//!
//! Halt is a predicate over the candidate set, not a scalar
//! threshold. Empty set ⇒ Success; top candidate requires an
//! external resolver ⇒ handoff; otherwise Execute. There is no
//! aggregate score gating the loop.

pub(crate) mod action;
pub(crate) mod ci;
pub(crate) mod claude_review;
pub(crate) mod closeout;
mod codex_review;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod decision;
pub(crate) mod doc_review;
pub(crate) mod pull_request_metadata;
pub(crate) mod reviews;
pub(crate) mod state;

use std::path::Path;

use crate::observe::github::compare::MergeBaseDelta;
use crate::orient::ci::CiReport;
use crate::orient::claude_review::ClaudeReview;
use crate::orient::closeout::Closeout;
use crate::orient::codex_review::CodexReviewReport;
use crate::orient::copilot::CopilotReport;
use crate::orient::cursor::CursorReport;
use crate::orient::doc_review::DocReview;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use crate::orient::reviews::ReviewSummary;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::ReviewThread;

use action::{Action, TargetEffect};

// `ActionEffect` and `Urgency` are referenced only by tests in this
// module; gate the imports to suppress the unused-import lint.
#[cfg(test)]
use action::{ActionEffect, Urgency};

/// Per-consumer input slice for [`candidates`]. Each field declares
/// a typed dep ref. The struct is the function signature reified;
/// scope is exactly what the aggregator reads across all axes
/// (the 9 PR-side axes plus the codex-review axis specific to this
/// binary).
pub(crate) struct CandidatesInputs<'a> {
    pub state: &'a PullRequestProjection,
    pub ci: &'a CiReport,
    pub reviews: &'a ReviewSummary,
    pub copilot: Option<&'a CopilotReport>,
    pub cursor: Option<&'a CursorReport>,
    pub codex_review: Option<&'a CodexReviewReport>,
    pub threads: &'a [ReviewThread],
    pub merge_base_delta: Option<&'a MergeBaseDelta>,
    pub pull_request_metadata: &'a PullRequestMetadata,
    pub attest_path: Option<&'a Path>,
    pub doc_review: &'a DocReview,
    pub doc_review_attest_path: Option<&'a Path>,
    pub claude_review: &'a ClaudeReview,
    pub claude_review_attest_path: Option<&'a Path>,
    pub closeout: &'a Closeout,
    pub closeout_attest_path: Option<&'a Path>,
}

impl<'a> From<&'a crate::orient::OrientedState> for CandidatesInputs<'a> {
    fn from(o: &'a crate::orient::OrientedState) -> Self {
        Self {
            state: &o.state,
            ci: &o.ci,
            reviews: &o.reviews,
            copilot: o.copilot.as_ref(),
            cursor: o.cursor.as_ref(),
            codex_review: o.codex_review.as_ref(),
            threads: &o.threads,
            merge_base_delta: o.merge_base_delta.as_ref(),
            pull_request_metadata: &o.pull_request_metadata,
            attest_path: o.attest_path.as_deref(),
            doc_review: &o.doc_review,
            doc_review_attest_path: o.doc_review_attest_path.as_deref(),
            claude_review: &o.claude_review,
            claude_review_attest_path: o.claude_review_attest_path.as_deref(),
            closeout: &o.closeout,
            closeout_attest_path: o.closeout_attest_path.as_deref(),
        }
    }
}

/// Generate the ranked candidate set across all axes.
///
/// Composition: collect from each axis (each owns its internal
/// ordering), then stable-sort by urgency. Stable sort preserves
/// axis order within an urgency tier so a higher-priority axis is
/// not displaced by a lower-priority one when they coincide.
///
/// Class invariant — *advancement preempts passivity*: an active
/// candidate the system can drive must outrank a candidate that
/// only waits on an external signal. Without it, a passive
/// per-axis wait can shadow a driver-side action and leave free
/// progress on the table.
pub(crate) fn candidates(
    inputs: &CandidatesInputs<'_>,
    pr: crate::ids::PullRequestNumber,
) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    // Mechanical merge-shape blockers precede CI: required checks
    // may not even start until the PR is in the correct lifecycle
    // shape, and CI failures on a conflicted branch are noise
    // until the merge base is resolved.
    out.extend(state::blocking_candidates(
        inputs.state,
        inputs.threads,
        inputs.merge_base_delta,
    ));
    out.extend(ci::candidates(inputs.ci));
    out.extend(reviews::candidates(
        inputs.reviews,
        inputs.ci,
        inputs.copilot,
        inputs.threads,
    ));
    if let Some(c) = inputs.copilot {
        out.extend(copilot::candidates(c));
    }
    if let Some(c) = inputs.cursor {
        out.extend(cursor::candidates(c));
    }
    if let Some(c) = inputs.codex_review {
        out.extend(codex_review::candidates(c));
    }
    // Hygiene-tier attestation axes append after the mechanical
    // and health axes. The urgency sort settles their relative
    // position; appending here keeps them out of the way of the
    // higher-tier candidates without losing the ability to fire
    // alone when they are the only outstanding axis.
    out.extend(pull_request_metadata::candidates(
        inputs.state,
        inputs.pull_request_metadata,
        inputs.attest_path,
        pr,
    ));
    out.extend(doc_review::candidates(
        inputs.state,
        inputs.doc_review,
        inputs.doc_review_attest_path,
        pr,
    ));
    out.extend(claude_review::candidates(
        inputs.claude_review,
        inputs.claude_review_attest_path,
        pr,
    ));
    // Closeout occupies the least-urgent tier — strictly below
    // every other axis. The urgency sort therefore selects it
    // only on global quiescence, which is precisely the condition
    // under which a pre-handoff sign-off makes sense.
    out.extend(closeout::candidates(
        inputs.closeout,
        inputs.closeout_attest_path,
        pr,
    ));
    // Fallback for an unmodeled merge gate: a Human handoff that
    // fires only when no other axis has produced an advancement
    // path. Suppression keys on whether any candidate either
    // unblocks (TargetEffect::Blocks) or advances the PR
    // (TargetEffect::Advances); a neutral hygiene candidate does
    // not count, because hygiene cannot clear a hard merge gate.
    let has_advancement_path = out.iter().any(|a| {
        matches!(
            a.target_effect,
            TargetEffect::Blocks | TargetEffect::Advances,
        )
    });
    if !has_advancement_path {
        out.extend(state::fallback_merge_state_blocker(inputs.state));
    }
    // Repo-policy hygiene (label vocabulary, description shape,
    // assignee conventions) is intentionally observed but not
    // decided on here: those conventions are project-specific, and
    // halting on them would block convergence on PRs that simply
    // don't share the convention. The fields are still surfaced
    // for human-facing rendering.
    out.sort_by_key(|a| a.urgency);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::BlockerKey;
    use action::{ActionKind, TargetEffect};
    use ooda_core::MidTier;

    fn act(name: &str, urgency: Urgency) -> Action {
        Action {
            kind: ActionKind::RequestApproval,
            effect: ActionEffect::Human {
                prompt: ooda_core::HandoffPrompt::new(name),
            },
            target_effect: TargetEffect::Blocks,
            urgency,
            blocker: BlockerKey::for_test(name),
        }
    }

    #[test]
    fn urgency_total_order_matches_design_intent() {
        // The total order on `Urgency` IS the priority lattice.
        // New tiers slot in by definition; the sort never changes.
        assert!(Urgency::Mid(MidTier::Critical) < Urgency::Mid(MidTier::BlockingFix));
        assert!(Urgency::Mid(MidTier::BlockingFix) < Urgency::Mid(MidTier::BlockingWait));
        assert!(Urgency::Mid(MidTier::BlockingWait) < Urgency::Mid(MidTier::BlockingHuman));
        assert!(Urgency::Mid(MidTier::BlockingHuman) < Urgency::Mid(MidTier::Advancing));
        assert!(Urgency::Mid(MidTier::Advancing) < Urgency::Mid(MidTier::Hygiene));
    }

    #[test]
    fn priority_sort_is_stable_within_urgency() {
        // Equal-urgency actions keep their input (axis) order
        // after sorting — the stability witness for the rank step.
        let mut v = [
            act("fix-a", Urgency::Mid(MidTier::BlockingFix)),
            act("wait", Urgency::Mid(MidTier::BlockingWait)),
            act("fix-b", Urgency::Mid(MidTier::BlockingFix)),
            act("critical", Urgency::Mid(MidTier::Critical)),
            act("human", Urgency::Mid(MidTier::BlockingHuman)),
            act("hygiene", Urgency::Mid(MidTier::Hygiene)),
        ];
        v.sort_by_key(|a| a.urgency);
        let order: Vec<&str> = v.iter().map(|a| a.blocker.as_str()).collect();
        assert_eq!(
            order,
            vec!["critical", "fix-a", "fix-b", "wait", "human", "hygiene"]
        );
    }
}
