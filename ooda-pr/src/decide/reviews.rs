//! Review candidates: address threads, wait on pending reviewers,
//! request approval.

use crate::observe::github::pr_view::ReviewDecision;
use crate::orient::copilot::{CopilotActivity, CopilotReport};
use crate::orient::cursor::CursorReport;
use crate::orient::OrientedState;

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};

pub fn candidates(oriented: &OrientedState) -> Vec<Action> {
    let reviews = &oriented.reviews;
    let ci = &oriented.ci;
    let copilot = oriented.copilot.as_ref();
    let cursor = oriented.cursor.as_ref();
    let mut out: Vec<Action> = Vec::new();

    if reviews.threads_unresolved > 0 {
        out.push(Action {
            kind: ActionKind::AddressThreads {
                count: reviews.threads_unresolved as u32,
            },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: address_threads_description(
                reviews.threads_unresolved as u32,
                copilot,
                cursor,
            ),
            // Stable key — count lives in ActionKind, never in the
            // blocker string, so 3→2 progress doesn't mask as stall.
            blocker: "unresolved_threads".into(),
        });
    }

    if !reviews.pending_reviews.bots.is_empty() {
        out.push(Action {
            kind: ActionKind::WaitForBotReview {
                reviewers: reviews.pending_reviews.bots.clone(),
            },
            automation: Automation::Wait { seconds: 60 },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            description: format!(
                "Wait for bot review from {}",
                reviews.pending_reviews.bots.join(", ")
            ),
            blocker: format!(
                "pending_bot_review: {}",
                reviews.pending_reviews.bots.join(", ")
            ),
        });
    }

    if !reviews.pending_reviews.humans.is_empty() {
        out.push(Action {
            kind: ActionKind::WaitForHumanReview {
                reviewers: reviews.pending_reviews.humans.clone(),
            },
            automation: Automation::Human,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            description: format!(
                "Waiting on human review from {}",
                reviews.pending_reviews.humans.join(", ")
            ),
            blocker: format!(
                "pending_human_review: {}",
                reviews.pending_reviews.humans.join(", ")
            ),
        });
    }

    // Approval request only when nothing else is in flight AND
    // the only missing thing is approval. ChangesRequested is
    // explicitly NOT a request-approval state — the reviewer
    // needs the changes addressed and a re-review, not another
    // approve click. Summary-only change requests (no thread
    // payload) would otherwise mis-route through this branch.
    let needs_approval = matches!(reviews.decision, Some(ReviewDecision::ReviewRequired));
    let ci_clean = ci.required.fail() == 0 && ci.required.pending() == 0;
    let threads_clean = reviews.threads_unresolved == 0;
    if needs_approval && ci_clean && threads_clean {
        out.push(Action {
            kind: ActionKind::RequestApproval,
            automation: Automation::Human,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            description: "Request or self-approve".into(),
            blocker: "not_approved".into(),
        });
    }

    // Summary-only change request: ChangesRequested with no inline
    // threads means the reviewer left feedback in the review body
    // (or all threads were resolved without re-approval). Without
    // this candidate, decide() would see an empty action set and
    // halt Success on a still-blocked PR. Class invariant: every
    // blocking ReviewDecision must produce a candidate.
    //
    // Suppress when ANY reviewer is already pending re-review: the
    // change has been addressed and a re-request is outstanding.
    // Without this gate, AddressChangeRequest (BlockingFix) shadows
    // WaitForHumanReview / WaitForBotReview (BlockingHuman/Wait) on
    // the urgency sort and the loop hands work back to the agent
    // even though the right next action is to wait for the
    // re-review.
    let changes_requested =
        matches!(reviews.decision, Some(ReviewDecision::ChangesRequested));
    let no_pending_re_review = reviews.pending_reviews.bots.is_empty()
        && reviews.pending_reviews.humans.is_empty();
    if changes_requested && threads_clean && no_pending_re_review {
        out.push(Action {
            kind: ActionKind::AddressChangeRequest,
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: address_change_request_description(),
            blocker: "changes_requested_summary".into(),
        });
    }

    out
}

fn address_change_request_description() -> String {
    "Address summary-only change-request review (no inline threads). \
     Read the latest CHANGES_REQUESTED review body via `gh pr view \
     --json reviews` and address the requested changes. \
     For each issue, think deeply about the entire class of issue, in \
     general, and solve the general form of the issue across all relevant \
     code. This ensures the entire category of each issue is solved in \
     general."
        .into()
}

/// Build the address_threads prompt with per-bot context when
/// available. Symmetric shape: "<Bot>: <facts>." then the
/// generalization directive.
fn address_threads_description(
    total: u32,
    copilot: Option<&CopilotReport>,
    cursor: Option<&CursorReport>,
) -> String {
    let mut parts: Vec<String> =
        vec![format!("Address {total} unresolved review thread(s).")];

    if let Some(c) = cursor
        && c.threads.unresolved > 0
    {
        let sev = c.severity.nonzero_parts();
        if !sev.is_empty() {
            parts.push(format!("Cursor: {}.", sev.join(", ")));
        }
    }

    if let Some(c) = copilot
        && let CopilotActivity::Reviewed { latest } = &c.activity
    {
        let issues = c.threads.unresolved;
        let suppressed = latest.comments_suppressed;
        let mut bits: Vec<String> = Vec::new();
        if issues > 0 {
            bits.push(format!("{issues} issue(s)"));
        }
        if suppressed > 0 {
            bits.push(format!("{suppressed} low-confidence finding(s)"));
        }
        if !bits.is_empty() {
            parts.push(format!("Copilot: {}.", bits.join(", ")));
        }
    }

    parts.push(
        "For each issue, think deeply about the entire class of issue, in \
         general, and solve the general form of the issue across all relevant \
         code. This ensures the entire category of each issue is solved in \
         general."
            .into(),
    );

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Timestamp;
    use crate::observe::github::pr_view::Mergeable;
    use crate::orient::ci::{CheckBucket, CiSummary};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestState;

    fn clean_ci() -> CiSummary {
        CiSummary {
            required: CheckBucket::default(),
            missing_names: vec![],
            completed_at: None,
            advisory: CheckBucket::default(),
        }
    }

    fn clean_reviews() -> ReviewSummary {
        ReviewSummary {
            decision: None,
            threads_unresolved: 0,
            threads_total: 0,
            bot_comments: 0,
            approvals_on_head: 0,
            approvals_stale: 0,
            pending_reviews: PendingReviews::default(),
            bot_reviews: vec![],
        }
    }

    fn clean_state() -> PullRequestState {
        PullRequestState {
            conflict: Mergeable::Mergeable,
            draft: false,
            wip: false,
            title_len: 30,
            title_ok: true,
            body: true,
            summary: true,
            test_plan: true,
            content_label: true,
            assignees: 1,
            reviewers: 1,
            merge_when_ready: false,
            commits: 1,
            behind: false,
            merge_state_status:
                crate::observe::github::pr_view::MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
        }
    }

    fn oriented_with(reviews: ReviewSummary) -> OrientedState {
        OrientedState {
            ci: clean_ci(),
            state: clean_state(),
            reviews,
            copilot: None,
            cursor: None,
        }
    }

    #[test]
    fn clean_reviews_yield_no_candidates() {
        assert!(candidates(&oriented_with(clean_reviews())).is_empty());
    }

    #[test]
    fn unresolved_threads_emit_address_threads() {
        let mut r = clean_reviews();
        r.threads_unresolved = 3;
        let cs = candidates(&oriented_with(r));
        assert!(matches!(cs[0].kind, ActionKind::AddressThreads { count: 3 }));
        assert_eq!(cs[0].automation, Automation::Agent);
    }

    #[test]
    fn pending_humans_marked_human_automation() {
        let mut r = clean_reviews();
        r.pending_reviews.humans = vec!["alice".into()];
        let cs = candidates(&oriented_with(r));
        let h = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::WaitForHumanReview { .. }))
            .unwrap();
        assert_eq!(h.automation, Automation::Human);
    }

    #[test]
    fn approval_request_only_when_ci_and_threads_clean() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ReviewRequired);
        let cs = candidates(&oriented_with(r.clone()));
        assert!(cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RequestApproval)));

        r.threads_unresolved = 1;
        let cs = candidates(&oriented_with(r));
        assert!(!cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RequestApproval)));
    }

    #[test]
    fn no_approval_when_decision_is_none() {
        let mut r = clean_reviews();
        r.decision = None;
        let cs = candidates(&oriented_with(r));
        assert!(!cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RequestApproval)));
    }

    #[test]
    fn summary_only_change_request_emits_address_change_request() {
        // ChangesRequested with no unresolved threads = summary-only
        // change request (or all threads resolved without re-approval).
        // Without this candidate, decide() would halt Success on a
        // still-blocked PR.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.threads_unresolved = 0;
        let cs = candidates(&oriented_with(r));
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest)),
            "ChangesRequested with no threads must emit AddressChangeRequest, got {cs:?}"
        );
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
            .unwrap();
        assert_eq!(action.automation, Automation::Agent);
        assert_eq!(action.target_effect, TargetEffect::Blocks);
        assert_eq!(action.blocker, "changes_requested_summary");
    }

    #[test]
    fn changes_requested_with_threads_does_not_double_emit() {
        // When threads exist, AddressThreads handles the work. The
        // summary-only candidate is gated on threads_clean to avoid
        // emitting two redundant blockers for the same review state.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.threads_unresolved = 2;
        let cs = candidates(&oriented_with(r));
        assert!(cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. })));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest)),
            "AddressChangeRequest must not fire when AddressThreads already covers the work"
        );
    }

    #[test]
    fn approved_decision_does_not_emit_change_request() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::Approved);
        let cs = candidates(&oriented_with(r));
        assert!(!cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest)));
    }

    #[test]
    fn change_request_fires_even_when_ci_failing() {
        // CI status is independent of the review-state class invariant.
        // A summary-only change request is a blocker regardless of CI.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let mut o = oriented_with(r);
        o.ci.required.failed = vec![crate::orient::ci::FailedCheck {
            name: "Lint".into(),
            description: String::new(),
            link: String::new(),
        }];
        let cs = candidates(&o);
        assert!(cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest)));
    }

    #[test]
    fn change_request_suppressed_when_re_review_pending() {
        // The change has been addressed and a re-review is pending.
        // AddressChangeRequest (BlockingFix) would otherwise sort
        // ahead of WaitForHumanReview/WaitForBotReview and send the
        // loop back to the agent unnecessarily. The pending wait
        // covers it.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.pending_reviews.humans = vec!["alice".into()];
        let cs = candidates(&oriented_with(r));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest)),
            "AddressChangeRequest must not fire when a re-review is pending, got {cs:?}"
        );
        // The wait still fires.
        assert!(cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::WaitForHumanReview { .. })));
    }
}
