//! Review candidates: address threads, wait on pending reviewers,
//! request approval.

use crate::ids::BlockerKey;
use std::time::Duration;

use crate::observe::github::pr_view::ReviewDecision;
use crate::orient::OrientedState;
use crate::orient::thread::{ReviewThread, ThreadAuthor, ThreadState};

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

    let unresolved_threads: Vec<ReviewThread> = oriented
        .threads
        .iter()
        .filter(|t| t.state != ThreadState::Resolved)
        .cloned()
        .collect();

    if !unresolved_threads.is_empty() {
        let description = address_threads_description(&unresolved_threads);
        out.push(Action {
            kind: ActionKind::AddressThreads {
                threads: unresolved_threads,
            },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description,
            // Stable key — the action carries the witness; the
            // blocker remains a fixed tag so 3→2 progress doesn't
            // mask as stall. Live and Outdated threads share this
            // tag because both require per-thread agent judgment;
            // the per-thread state is carried through in the prompt.
            blocker: BlockerKey::tag("unresolved_threads"),
        });
    }

    if !reviews.pending_reviews.bots.is_empty() {
        let names = join_display(&reviews.pending_reviews.bots);
        out.push(Action {
            kind: ActionKind::WaitForBotReview {
                reviewers: reviews.pending_reviews.bots.clone(),
            },
            automation: Automation::Wait {
                interval: Duration::from_secs(60),
            },
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
    // Symmetric with the AddressThreads filter: a thread that is
    // Outdated but not Resolved still requires per-thread agent
    // judgment (anchor moved, but the logical feedback may still
    // apply), so RequestApproval / AddressChangeRequest must wait
    // until every thread has reached the Resolved state.
    let threads_clean = !oriented
        .threads
        .iter()
        .any(|t| t.state != ThreadState::Resolved);
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
    let changes_requested = matches!(reviews.decision, Some(ReviewDecision::ChangesRequested));
    let no_pending_re_review =
        reviews.pending_reviews.bots.is_empty() && reviews.pending_reviews.humans.is_empty();
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
/// 1. Headline count, with a Live/Outdated breakdown when the set
///    is mixed.
/// 2. Per-author breakdown.
/// 3. Numbered threads with location, optional `[outdated]` tag,
///    `thread_id` (for the resolve mutation), and full body.
/// 4. The generalization directive (class-of-issue) plus the
///    verify-then-act-or-resolve directive for outdated threads.
///
/// The actor receives the prompt material directly — no second
/// `gh api graphql` round-trip required to discover what to fix.
fn address_threads_description(threads: &[ReviewThread]) -> String {
    let outdated_count = threads
        .iter()
        .filter(|t| t.state == ThreadState::Outdated)
        .count();
    let live_count = threads.len() - outdated_count;

    let headline = if outdated_count > 0 && live_count > 0 {
        format!(
            "Address {} ({} live, {} outdated).",
            crate::text::count(threads.len(), "unresolved review thread"),
            live_count,
            outdated_count,
        )
    } else if outdated_count > 0 {
        format!(
            "Address {} (all outdated; anchors moved since the comments were authored).",
            crate::text::count(threads.len(), "unresolved review thread"),
        )
    } else {
        format!(
            "Address {}.",
            crate::text::count(threads.len(), "unresolved review thread"),
        )
    };
    let mut parts: Vec<String> = vec![headline];

    let by_author = count_by_author(threads);
    if !by_author.is_empty() {
        let bits: Vec<String> = by_author
            .iter()
            .map(|(author, count)| format!("{}: {}", author, crate::text::count(*count, "issue")))
            .collect();
        parts.push(format!("{}.", bits.join(" · ")));
    }

    parts.push(String::new());
    for (i, t) in threads.iter().enumerate() {
        let tag = match t.state {
            ThreadState::Outdated => "    [outdated]",
            // Live and Resolved both render without a tag; Resolved
            // is structurally excluded by the caller's filter, so
            // only Live reaches this arm.
            _ => "",
        };
        parts.push(format!(
            "{}. {} @ {}{}    thread_id: {}",
            i + 1,
            t.author,
            t.location,
            tag,
            t.id,
        ));
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

    if outdated_count > 0 {
        parts.push(String::new());
        parts.push(
            "For threads marked [outdated]: GitHub's `isOutdated` flag is \
             positional, not content-relevance — the diff hunk that anchored \
             the thread has moved (typically due to a refactor or rebase), \
             so the comment no longer renders inline, but the logical \
             feedback may still apply to the current code. For each outdated \
             thread, locate the current code that the comment is about (often \
             near the original `path:line` after a small refactor; sometimes \
             elsewhere) and decide whether the feedback still applies. If it \
             does, address it as you would a live thread. If it does not, \
             resolve the thread with a brief reply explaining why."
                .into(),
        );
    }

    parts.push(String::new());
    parts.push(
        "After addressing (or judging not-applicable) each thread, mark it \
         resolved on GitHub:"
            .into(),
    );
    parts.push(
        "  gh api graphql -f query='mutation { resolveReviewThread(input: \
         { threadId: \"<thread_id>\" }) { thread { id } } }'"
            .into(),
    );
    parts.push(
        "(Substitute the per-thread `thread_id` shown in each entry above. \
         The mutation is idempotent — already-resolved threads succeed as a \
         no-op.)"
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
            merge_state_status: crate::observe::github::pr_view::MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
        }
    }

    fn oriented_with(reviews: ReviewSummary) -> OrientedState {
        oriented_with_threads(reviews, vec![])
    }

    fn oriented_with_threads(reviews: ReviewSummary, threads: Vec<ReviewThread>) -> OrientedState {
        OrientedState {
            ci: clean_ci(),
            state: clean_state(),
            reviews,
            copilot: None,
            cursor: None,
            threads,
            codex_review: None,
        }
    }

    fn thread_in_state(
        path: &str,
        line: u32,
        body: &str,
        state: ThreadState,
        id: &str,
    ) -> ReviewThread {
        use crate::orient::thread::{BotName, FilePath, ThreadId, ThreadLocation};
        ReviewThread {
            id: ThreadId::new(id.to_string()),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new(path),
                line: Some(line),
            },
            body: body.into(),
            state,
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
        }
    }

    fn live_thread(path: &str, line: u32, body: &str) -> ReviewThread {
        thread_in_state(path, line, body, ThreadState::Live, "t")
    }

    fn outdated_thread(path: &str, line: u32, body: &str, id: &str) -> ReviewThread {
        thread_in_state(path, line, body, ThreadState::Outdated, id)
    }

    fn resolved_thread(path: &str, line: u32, body: &str, id: &str) -> ReviewThread {
        thread_in_state(path, line, body, ThreadState::Resolved, id)
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
    fn outdated_unresolved_threads_emit_address_threads() {
        // Bug fix: GitHub's isOutdated is positional, not
        // content-relevance. Outdated unresolved threads still need
        // per-thread agent judgment and must reach AddressThreads.
        let r = clean_reviews();
        let threads = vec![outdated_thread(
            "src/foo.rs",
            42,
            "still wrong even after move",
            "T_outdated",
        )];
        let cs = candidates(&oriented_with_threads(r, threads));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads must fire on outdated unresolved threads");
        assert_eq!(action.automation, Automation::Agent);
    }

    #[test]
    fn mixed_live_and_outdated_threads_share_one_action() {
        let r = clean_reviews();
        let threads = vec![
            live_thread("src/a.rs", 1, "live concern"),
            outdated_thread("src/b.rs", 2, "outdated concern", "T_outdated"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads must fire");
        match &action.kind {
            ActionKind::AddressThreads { threads } => assert_eq!(threads.len(), 2),
            _ => unreachable!(),
        }
        // Headline carries the live/outdated breakdown.
        assert!(action.description.contains("(1 live, 1 outdated)"));
        // Outdated entry tagged; live entry not.
        assert!(action.description.contains("[outdated]"));
        // thread_id surfaced for the resolve mutation.
        assert!(action.description.contains("T_outdated"));
        // Verify-then-act-or-resolve clause appears for outdated.
        assert!(action.description.contains("isOutdated` flag is"));
        // Resolve mutation template appears unconditionally (any
        // unresolved set ends with the resolve instruction).
        assert!(action.description.contains("resolveReviewThread"));
    }

    #[test]
    fn all_outdated_set_uses_all_outdated_headline() {
        let r = clean_reviews();
        let threads = vec![
            outdated_thread("src/a.rs", 1, "first", "T_a"),
            outdated_thread("src/b.rs", 2, "second", "T_b"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        let desc = &cs[0].description;
        assert!(desc.contains("Address 2 unresolved review threads (all outdated"));
        // Live/outdated breakdown only appears when mixed.
        assert!(!desc.contains("0 live"));
    }

    #[test]
    fn resolved_threads_excluded_from_action() {
        let r = clean_reviews();
        let threads = vec![
            resolved_thread("src/a.rs", 1, "already done", "T_a"),
            resolved_thread("src/b.rs", 2, "also done", "T_b"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. })),
            "All-Resolved set must not emit AddressThreads"
        );
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
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );

        let threads = vec![live_thread("src/a.rs", 1, "x")];
        let cs = candidates(&oriented_with_threads(r, threads));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );
    }

    #[test]
    fn approval_blocked_by_outdated_unresolved_threads() {
        // Symmetric with the AddressThreads filter: outdated-but-
        // not-resolved threads still need agent judgment, so
        // RequestApproval must wait until the entire set is
        // Resolved.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ReviewRequired);
        let threads = vec![outdated_thread("src/a.rs", 1, "x", "T_o")];
        let cs = candidates(&oriented_with_threads(r, threads));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval)),
            "RequestApproval must not fire while outdated unresolved threads remain"
        );
        // AddressThreads fires instead.
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
        );
    }

    #[test]
    fn approval_fires_when_all_threads_resolved() {
        // The symmetric gate also admits the all-Resolved case:
        // every thread is closed, AddressThreads is silent,
        // RequestApproval can fire.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ReviewRequired);
        let threads = vec![resolved_thread("src/a.rs", 1, "done", "T_r")];
        let cs = candidates(&oriented_with_threads(r, threads));
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );
    }

    #[test]
    fn no_approval_when_decision_is_none() {
        let mut r = clean_reviews();
        r.decision = None;
        let cs = candidates(&oriented_with(r));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );
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
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
        );
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
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
        );
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
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
        );
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
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::WaitForHumanReview { .. }))
        );
    }
}
