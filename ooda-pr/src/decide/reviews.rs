//! Review candidates: address threads, wait on pending reviewers,
//! request approval.

use crate::ids::BlockerKey;
use std::time::Duration;

use crate::observe::github::pr_view::ReviewDecision;
use crate::orient::thread::{ReviewThread, ThreadAuthor, ThreadState};
use crate::orient::OrientedState;

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};

/// Comma-join a slice of any `Display` for human-readable rendering.
fn join_display<T: std::fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(T::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn candidates(oriented: &OrientedState) -> Vec<Action> {
    let reviews = &oriented.reviews;
    let ci = &oriented.ci;
    let mut out: Vec<Action> = Vec::new();

    let live_threads: Vec<ReviewThread> = oriented
        .threads
        .iter()
        .filter(|t| t.state == ThreadState::Live)
        .cloned()
        .collect();

    if !live_threads.is_empty() {
        let description = address_threads_description(&live_threads);
        out.push(Action {
            kind: ActionKind::AddressThreads { threads: live_threads },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description,
            // Stable key — the action carries the witness; the
            // blocker remains a fixed tag so 3→2 progress doesn't
            // mask as stall.
            blocker: BlockerKey::tag("unresolved_threads"),
        });
    }

    if !reviews.pending_reviews.bots.is_empty() {
        let names = join_display(&reviews.pending_reviews.bots);
        out.push(Action {
            kind: ActionKind::WaitForBotReview {
                reviewers: reviews.pending_reviews.bots.clone(),
            },
            automation: Automation::Wait { interval: Duration::from_secs(60) },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            description: format!("Wait for bot review from {names}"),
            blocker: BlockerKey::tag(format!("pending_bot_review: {names}")),
        });
    }

    if !reviews.pending_reviews.humans.is_empty() {
        let names = join_display(&reviews.pending_reviews.humans);
        out.push(Action {
            kind: ActionKind::WaitForHumanReview {
                reviewers: reviews.pending_reviews.humans.clone(),
            },
            automation: Automation::Human,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            description: format!("Waiting on human review from {names}"),
            blocker: BlockerKey::tag(format!("pending_human_review: {names}")),
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
    let threads_clean = !oriented
        .threads
        .iter()
        .any(|t| t.state == ThreadState::Live);
    if needs_approval && ci_clean && threads_clean {
        out.push(Action {
            kind: ActionKind::RequestApproval,
            automation: Automation::Human,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            description: "Request or self-approve".into(),
            blocker: BlockerKey::tag("not_approved"),
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
            blocker: BlockerKey::tag("changes_requested_summary"),
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

/// Build the address_threads prompt with the threads themselves
/// inlined as witnesses. Structure:
///
/// 1. Headline count
/// 2. Per-author breakdown
/// 3. Numbered threads with location and full body
/// 4. The generalization directive
///
/// The actor receives the prompt material directly — no second
/// `gh api graphql` round-trip required to discover what to fix.
fn address_threads_description(threads: &[ReviewThread]) -> String {
    let mut parts: Vec<String> = vec![format!(
        "Address {}.",
        crate::text::count(threads.len(), "unresolved review thread"),
    )];

    let by_author = count_by_author(threads);
    if !by_author.is_empty() {
        let bits: Vec<String> = by_author
            .iter()
            .map(|(author, count)| {
                format!("{}: {}", author, crate::text::count(*count, "issue"))
            })
            .collect();
        parts.push(format!("{}.", bits.join(" · ")));
    }

    parts.push(String::new());
    for (i, t) in threads.iter().enumerate() {
        parts.push(format!("{}. {} @ {}", i + 1, t.author, t.location));
        for line in t.body.lines() {
            parts.push(format!("   > {line}"));
        }
        parts.push(String::new());
    }

    parts.push(
        "For each issue, think deeply about the entire class of issue, in \
         general, and solve the general form of the issue across all relevant \
         code. This ensures the entire category of each issue is solved in \
         general."
            .into(),
    );

    parts.join("\n")
}

/// Group threads by author preserving first-seen order. Linear scan
/// — sufficient for the realistic case (a handful of authors per
/// PR) and avoids requiring `Hash`/`Ord` on the author sum type.
fn count_by_author(threads: &[ReviewThread]) -> Vec<(ThreadAuthor, usize)> {
    let mut counts: Vec<(ThreadAuthor, usize)> = Vec::new();
    for t in threads {
        if let Some((_, c)) = counts.iter_mut().find(|(a, _)| a == &t.author) {
            *c += 1;
        } else {
            counts.push((t.author.clone(), 1));
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitHubLogin, Reviewer, Timestamp};
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
        oriented_with_threads(reviews, vec![])
    }

    fn oriented_with_threads(
        reviews: ReviewSummary,
        threads: Vec<ReviewThread>,
    ) -> OrientedState {
        OrientedState {
            ci: clean_ci(),
            state: clean_state(),
            reviews,
            copilot: None,
            cursor: None,
            threads,
        }
    }

    fn live_thread(path: &str, line: u32, body: &str) -> ReviewThread {
        use crate::orient::thread::{
            BotName, FilePath, ThreadId, ThreadLocation,
        };
        ReviewThread {
            id: ThreadId::new("t".to_string()),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new(path),
                line: Some(line),
            },
            body: body.into(),
            state: ThreadState::Live,
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
        }
    }

    #[test]
    fn clean_reviews_yield_no_candidates() {
        assert!(candidates(&oriented_with(clean_reviews())).is_empty());
    }

    #[test]
    fn unresolved_threads_emit_address_threads() {
        let r = clean_reviews();
        let threads = vec![
            live_thread("src/a.rs", 1, "first"),
            live_thread("src/b.rs", 2, "second"),
            live_thread("src/c.rs", 3, "third"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        match &cs[0].kind {
            ActionKind::AddressThreads { threads } => {
                assert_eq!(threads.len(), 3);
            }
            other => panic!("expected AddressThreads, got {other:?}"),
        }
        assert_eq!(cs[0].automation, Automation::Agent);
    }

    #[test]
    fn address_threads_description_inlines_thread_bodies() {
        let r = clean_reviews();
        let threads = vec![
            live_thread("src/foo.rs", 42, "unwrap should be ?"),
            live_thread("src/bar.rs", 99, "missing error context"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        let desc = &cs[0].description;
        // Headline + per-author breakdown
        assert!(desc.contains("Address 2 unresolved review threads."));
        assert!(desc.contains("Copilot: 2 issues."));
        // Both witnesses inlined (location + body)
        assert!(desc.contains("Copilot @ src/foo.rs:42"));
        assert!(desc.contains("> unwrap should be ?"));
        assert!(desc.contains("Copilot @ src/bar.rs:99"));
        assert!(desc.contains("> missing error context"));
        // Generalization preamble preserved
        assert!(desc.contains("think deeply about the entire class of issue"));
    }

    #[test]
    fn pending_humans_marked_human_automation() {
        let mut r = clean_reviews();
        r.pending_reviews.humans = vec![Reviewer::User(GitHubLogin::parse("alice").unwrap())];
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

        let threads = vec![live_thread("src/a.rs", 1, "x")];
        let cs = candidates(&oriented_with_threads(r, threads));
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
        assert_eq!(action.blocker.as_str(), "changes_requested_summary");
    }

    #[test]
    fn changes_requested_with_threads_does_not_double_emit() {
        // When threads exist, AddressThreads handles the work. The
        // summary-only candidate is gated on threads_clean to avoid
        // emitting two redundant blockers for the same review state.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let threads = vec![
            live_thread("src/a.rs", 1, "x"),
            live_thread("src/b.rs", 2, "y"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
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
            name: crate::ids::CheckName::parse("Lint").unwrap(),
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
        r.pending_reviews.humans = vec![Reviewer::User(GitHubLogin::parse("alice").unwrap())];
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
