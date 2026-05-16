//! Review candidates: address threads, wait on pending reviewers,
//! request approval.

use crate::ids::{BlockerKey, Timestamp};

use crate::observe::github::pull_request_view::ReviewDecision;
use crate::orient::OrientedState;
use crate::orient::reviews::{HumanReview, ReviewSummary};
use crate::orient::thread::{ReviewThread, ThreadAuthor, ThreadState};

use super::action::{Action, ActionEffect, ActionKind, NonEmpty, TargetEffect, Urgency};
use ooda_core::{HandoffPrompt, SingleLineString, Witness};

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
    let ci = &oriented.ci.summary;
    let mut out: Vec<Action> = Vec::new();

    let unresolved_threads: Vec<ReviewThread> = oriented
        .threads
        .iter()
        .filter(|t| t.state != ThreadState::Resolved)
        .cloned()
        .collect();

    if let Some(unresolved_threads) = NonEmpty::try_from_vec(unresolved_threads) {
        let prompt = address_threads_prompt(&unresolved_threads);
        out.push(Action {
            kind: ActionKind::AddressThreads {
                threads: unresolved_threads,
            },
            effect: ActionEffect::Agent { prompt },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            // Stable key — the action carries the witness; the
            // blocker remains a fixed tag so 3→2 progress doesn't
            // mask as stall. Live and Outdated threads share this
            // tag because both require per-thread agent judgment;
            // the per-thread state is carried through in the prompt.
            blocker: BlockerKey::tag("unresolved_threads"),
        });
    }

    if let Some(bots) = NonEmpty::try_from_vec(reviews.pending_reviews.bots.clone()) {
        let names = join_display(&bots);
        out.push(Action {
            kind: ActionKind::WaitForBotReview { reviewers: bots },
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(60),
                log: format!("Wait for bot review from {names}"),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::tag(format!("pending_bot_review: {names}")),
        });
    }

    if let Some(humans) = NonEmpty::try_from_vec(reviews.pending_reviews.humans.clone()) {
        let names = join_display(&humans);
        out.push(Action {
            kind: ActionKind::WaitForHumanReview { reviewers: humans },
            effect: ActionEffect::Human {
                prompt: ooda_core::HandoffPrompt::new(format!(
                    "Waiting on human review from {names}"
                )),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
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
            effect: ActionEffect::Human {
                prompt: request_approval_prompt(reviews),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
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
            effect: ActionEffect::Agent {
                prompt: address_change_request_prompt(
                    reviews.latest_human_changes_requested.as_ref(),
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag("changes_requested_summary"),
        });
    }

    out
}

/// Build the `AddressChangeRequest` prompt. When the latest human
/// `CHANGES_REQUESTED` review is observed, inline its author, timestamp,
/// and full body as a `Witness` so the agent does not need a
/// `gh pr view --json reviews` round-trip to see what was asked. When
/// the projection found no human review (bots-only change request, or
/// a race between the GraphQL `review_decision` and the REST reviews
/// feed) the prompt falls back to the prior fetch-it-yourself
/// instruction.
fn address_change_request_prompt(latest: Option<&HumanReview>) -> HandoffPrompt {
    let mut prompt =
        HandoffPrompt::new("Address summary-only change-request review (no inline threads).");

    if let Some(h) = latest {
        let when = h
            .submitted_at
            .as_ref()
            .map_or_else(|| "unknown time".to_string(), Timestamp::to_string);
        let label = SingleLineString::new(format!("{} @ {when}", h.author));
        let body = if h.body.trim().is_empty() {
            "   > (review body was empty)".to_string()
        } else {
            h.body
                .lines()
                .map(|line| format!("   > {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        prompt.push_paragraph("Latest CHANGES_REQUESTED review:".to_string());
        prompt.push_witnesses(NonEmpty::singleton(Witness {
            label,
            body,
            url: None,
        }));
    } else {
        prompt.push_paragraph(
            "No human CHANGES_REQUESTED review observed in the reviews \
             projection (rare bot-only path or REST/GraphQL race).",
        );
        prompt
            .push_paragraph("Step 1 — fetch the latest CHANGES_REQUESTED review body:".to_string());
        prompt.push_paragraph("gh pr view --json reviews".to_string());
        prompt.push_paragraph("Step 2 — address the requested changes.".to_string());
    }

    prompt.push_paragraph(
        "For each issue, think deeply about the entire class of issue, in \
         general, and solve the general form of the issue across all relevant \
         code. This ensures the entire category of each issue is solved in \
         general.",
    );

    prompt
}

/// Build the `RequestApproval` prompt. Surfaces who must approve
/// (required reviewers from `requested_reviewers`) and the current
/// `approvals_on_head` ratio so the human handoff knows exactly which
/// approval signature is missing. When no required reviewers have
/// been recorded yet, the headline alone covers the situation —
/// CODEOWNERS- or branch-rule-derived requirements may not be present
/// on the `requested_reviewers` REST endpoint until GitHub fans them
/// in.
fn request_approval_prompt(reviews: &ReviewSummary) -> HandoffPrompt {
    let denom = reviews.requested_reviewers.bots.len() + reviews.requested_reviewers.humans.len();
    let headline = if denom == 0 {
        format!(
            "Request or self-approve ({}/? approvals on HEAD).",
            reviews.approvals_on_head,
        )
    } else {
        format!(
            "Request or self-approve ({}/{denom} approvals on HEAD).",
            reviews.approvals_on_head,
        )
    };
    let mut prompt = HandoffPrompt::new(headline);

    if !reviews.requested_reviewers.is_empty() {
        let names = reviews
            .requested_reviewers
            .all()
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        prompt.push_paragraph(format!("Required reviewers: {names}."));
    }
    if reviews.approvals_stale > 0 {
        prompt.push_paragraph(format!(
            "{} on prior HEADs — a new push invalidated those approvals.",
            crate::text::count(reviews.approvals_stale, "stale approval"),
        ));
    }

    prompt
}

/// Build the `AddressThreads` prompt with the threads themselves
/// inlined as witnesses. Structure:
///
/// * `headline` — count, with a Live/Outdated breakdown when the
///   set is mixed.
/// * Paragraph — per-author breakdown.
/// * Witnesses — one per thread; label = numbered location +
///   `[outdated]` tag + `thread_id`; body = quoted comment lines.
/// * Paragraph — class-of-issue generalization directive.
/// * Paragraph (optional) — verify-then-act-or-resolve directive
///   for the outdated subset.
/// * Paragraph — `resolveReviewThread` GraphQL template plus
///   idempotency note.
///
/// The actor receives the prompt material directly — no second
/// `gh api graphql` round-trip required to discover what to fix.
fn address_threads_prompt(threads: &NonEmpty<ReviewThread>) -> ooda_core::HandoffPrompt {
    use ooda_core::{HandoffPrompt, SingleLineString, Witness};

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

    let mut prompt = HandoffPrompt::new(headline);

    let by_author = count_by_author(threads);
    if !by_author.is_empty() {
        let bits: Vec<String> = by_author
            .iter()
            .map(|(author, count)| format!("{}: {}", author, crate::text::count(*count, "issue")))
            .collect();
        prompt.push_paragraph(format!("{}.", bits.join(" · ")));
    }

    // `enumerate_map_ref` preserves the non-empty invariant
    // structurally: input `NonEmpty<ReviewThread>` ⇒ output
    // `NonEmpty<Witness>`, no runtime check on cardinality.
    let witnesses = threads.enumerate_map_ref(|i, t| {
        let tag = match t.state {
            ThreadState::Outdated => "    [outdated]",
            // Live and Resolved both render without a tag;
            // Resolved is excluded by the caller's filter.
            _ => "",
        };
        let label = SingleLineString::new(format!(
            "{}. {} @ {}{}    thread_id: {}",
            i + 1,
            t.author,
            t.location,
            tag,
            t.id,
        ));
        let body = t
            .body
            .lines()
            .map(|line| format!("   > {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        Witness {
            label,
            body,
            url: None,
        }
    });
    prompt.push_witnesses(witnesses);

    prompt.push_paragraph(
        "Step 1 — for each issue, think deeply about the entire class of issue, \
         in general, and solve the general form of the issue across all relevant \
         code. This ensures the entire category of each issue is solved in \
         general, not just the specific instance the reviewer flagged.",
    );

    if outdated_count > 0 {
        prompt.push_paragraph(
            "Step 1a (outdated threads only) — GitHub's `isOutdated` flag is \
             positional, not content-relevance: the diff hunk that anchored the \
             thread has moved (typically due to a refactor or rebase), so the \
             comment no longer renders inline, but the logical feedback may \
             still apply to the current code. For each outdated thread, locate \
             the current code that the comment is about (often near the original \
             `path:line` after a small refactor; sometimes elsewhere) and decide \
             whether the feedback still applies. If it does, address it as you \
             would a live thread. If it does not, resolve the thread with a \
             brief reply explaining why.",
        );
    }

    prompt.push_paragraph(
        "Step 2 — after addressing (or judging not-applicable) each thread, mark \
         it resolved on GitHub by running:"
            .to_string(),
    );

    prompt.push_paragraph(
        "gh api graphql -f query='mutation { resolveReviewThread(input: \
         { threadId: \"<thread_id>\" }) { thread { id } } }'"
            .to_string(),
    );

    prompt.push_paragraph(
        "Substitute the per-thread `thread_id` shown in each entry above. The \
         mutation is idempotent — already-resolved threads succeed as a no-op."
            .to_string(),
    );

    prompt
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
    use crate::observe::github::pull_request_view::Mergeable;
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestProjection;

    fn clean_ci() -> CiReport {
        CiReport {
            summary: CiSummary {
                required: CheckBucket::default(),
                missing_names: vec![],
                completed_at: None,
                advisory: CheckBucket::default(),
            },
            activity: CiActivity::Resolved(ResolvedState::AllGreen),
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
            requested_reviewers: crate::orient::reviews::RequestedReviewerSet::default(),
            latest_human_changes_requested: None,
        }
    }

    fn clean_state() -> PullRequestProjection {
        PullRequestProjection {
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
            has_open_parent_pr: false,
            merge_state_status: crate::observe::github::pull_request_view::MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
            active_branch_rule_types: vec![],
            required_check_names_per_ruleset: vec![],
            missing_required_check_names_on_head: vec![],
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
            merge_base_delta: None,
            pull_request_metadata:
                crate::orient::pull_request_metadata::PullRequestMetadata::Synced,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::Synced,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
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
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn address_threads_description_inlines_thread_bodies() {
        let r = clean_reviews();
        let threads = vec![
            live_thread("src/foo.rs", 42, "unwrap should be ?"),
            live_thread("src/bar.rs", 99, "missing error context"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        let desc = &cs[0].rendered_payload();
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
        assert!(matches!(action.effect, ActionEffect::Agent { .. }));
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
        assert!(action.rendered_payload().contains("(1 live, 1 outdated)"));
        // Outdated entry tagged; live entry not.
        assert!(action.rendered_payload().contains("[outdated]"));
        // thread_id surfaced for the resolve mutation.
        assert!(action.rendered_payload().contains("T_outdated"));
        // Verify-then-act-or-resolve clause appears for outdated.
        assert!(action.rendered_payload().contains("isOutdated` flag is"));
        // Resolve mutation template appears unconditionally (any
        // unresolved set ends with the resolve instruction).
        assert!(action.rendered_payload().contains("resolveReviewThread"));
    }

    #[test]
    fn all_outdated_set_uses_all_outdated_headline() {
        let r = clean_reviews();
        let threads = vec![
            outdated_thread("src/a.rs", 1, "first", "T_a"),
            outdated_thread("src/b.rs", 2, "second", "T_b"),
        ];
        let cs = candidates(&oriented_with_threads(r, threads));
        let desc = &cs[0].rendered_payload();
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
        assert!(matches!(h.effect, ActionEffect::Human { .. }));
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
        assert!(matches!(action.effect, ActionEffect::Agent { .. }));
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
        o.ci.summary.required.failed = vec![crate::orient::ci::FailedCheck {
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

    // ─── property tests for the class invariant ─────────────────────
    //
    // Class invariant from `candidates`'s docs: "Every blocking
    // ReviewDecision must produce a candidate." In a clean baseline
    // state (no threads, no pending reviewers, no CI gates), the
    // review axis produces exactly one decision-derived candidate
    // — or none, for the non-blocking decisions.
    //
    // The exhaustive match in `expected_review_axis_behavior` is the
    // contract. Adding a new `ReviewDecision` variant fails to
    // compile here until the new arm is added.

    /// Decision-axis behavior in a clean baseline state. The axis
    /// also emits `WaitForBotReview` / `WaitForHumanReview` when
    /// pending reviewers exist — those are not decision-driven and
    /// are deliberately excluded by this property test's setup.
    #[derive(Debug, PartialEq, Eq)]
    enum ReviewAxisBehavior {
        /// No decision-derived candidate (decision is None or Approved).
        NoBlocker,
        /// `RequestApproval` — `ReviewRequired` with everything else
        /// clean: agent has nothing left to do, human must approve.
        EmitRequestApproval,
        /// `AddressChangeRequest` — `ChangesRequested` with no inline
        /// threads (summary-only review) and no pending re-review.
        EmitAddressChangeRequest,
    }

    /// Exhaustive over `Option<ReviewDecision>`. The compiler
    /// enforces that every variant has an explicit behavior arm.
    fn expected_review_axis_behavior(decision: Option<ReviewDecision>) -> ReviewAxisBehavior {
        // Intentional exhaustive match per axis pattern; arms are
        // duplicated for spec clarity.
        #[allow(clippy::match_same_arms)]
        match decision {
            None => ReviewAxisBehavior::NoBlocker,
            Some(ReviewDecision::Approved) => ReviewAxisBehavior::NoBlocker,
            Some(ReviewDecision::ReviewRequired) => ReviewAxisBehavior::EmitRequestApproval,
            Some(ReviewDecision::ChangesRequested) => ReviewAxisBehavior::EmitAddressChangeRequest,
        }
    }

    fn all_review_decisions() -> Vec<Option<ReviewDecision>> {
        vec![
            None,
            Some(ReviewDecision::Approved),
            Some(ReviewDecision::ReviewRequired),
            Some(ReviewDecision::ChangesRequested),
        ]
    }

    fn observed_review_axis_behavior(cs: &[Action]) -> ReviewAxisBehavior {
        let has_request_approval = cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RequestApproval));
        let has_address_change = cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest));
        match (has_request_approval, has_address_change) {
            (false, false) => ReviewAxisBehavior::NoBlocker,
            (true, false) => ReviewAxisBehavior::EmitRequestApproval,
            (false, true) => ReviewAxisBehavior::EmitAddressChangeRequest,
            (true, true) => {
                panic!("review axis emitted both RequestApproval and AddressChangeRequest")
            }
        }
    }

    #[test]
    fn review_axis_property_holds_for_every_decision() {
        let decisions = all_review_decisions();
        assert_eq!(
            decisions.len(),
            4,
            "`all_review_decisions` must include `None` plus one sample \
             per `ReviewDecision` variant; adding a new variant requires \
             adding both an arm in `expected_review_axis_behavior` AND a \
             sample here.",
        );
        for decision in decisions {
            let mut r = clean_reviews();
            r.decision = decision;
            let cs = candidates(&oriented_with(r));
            let actual = observed_review_axis_behavior(&cs);
            let expected = expected_review_axis_behavior(decision);
            assert_eq!(
                actual, expected,
                "review-axis contract violated for decision = {decision:?}",
            );
        }
    }

    // ── prompt-enrichment tests ─────────────────────────────────────

    #[test]
    fn request_approval_prompt_lists_required_reviewers_and_approval_ratio() {
        use crate::ids::TeamName;
        use crate::orient::reviews::RequestedReviewerSet;

        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ReviewRequired);
        r.approvals_on_head = 0;
        r.requested_reviewers = RequestedReviewerSet {
            bots: vec![GitHubLogin::parse("copilot[bot]").unwrap()],
            humans: vec![
                Reviewer::User(GitHubLogin::parse("alice").unwrap()),
                Reviewer::Team(TeamName::parse("backend").unwrap()),
            ],
        };
        let cs = candidates(&oriented_with(r));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::RequestApproval))
            .expect("RequestApproval must fire");
        let rendered = action.rendered_payload();
        assert!(
            rendered.contains("0/3 approvals on HEAD"),
            "missing approval ratio: {rendered}",
        );
        assert!(
            rendered.contains("Required reviewers: copilot[bot], alice, backend"),
            "missing reviewers list: {rendered}",
        );
    }

    #[test]
    fn request_approval_prompt_surfaces_stale_approvals() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ReviewRequired);
        r.approvals_stale = 2;
        let cs = candidates(&oriented_with(r));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::RequestApproval))
            .expect("RequestApproval must fire");
        let rendered = action.rendered_payload();
        assert!(
            rendered.contains("2 stale approvals on prior HEADs"),
            "missing stale-approvals line: {rendered}",
        );
    }

    #[test]
    fn request_approval_prompt_omits_reviewers_section_when_none_required() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ReviewRequired);
        let cs = candidates(&oriented_with(r));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::RequestApproval))
            .expect("RequestApproval must fire");
        let rendered = action.rendered_payload();
        assert!(
            rendered.contains("0/? approvals on HEAD"),
            "missing approval ratio with unknown denominator: {rendered}",
        );
        assert!(!rendered.contains("Required reviewers:"));
    }

    #[test]
    fn address_change_request_prompt_inlines_review_body_when_observed() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.latest_human_changes_requested = Some(HumanReview {
            author: GitHubLogin::parse("alice").unwrap(),
            submitted_at: Some(Timestamp::parse("2026-05-15T12:34:56Z").unwrap()),
            body: "Please factor the duplicated parsing logic\ninto a shared helper.".into(),
        });
        let cs = candidates(&oriented_with(r));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
            .expect("AddressChangeRequest must fire");
        let rendered = action.rendered_payload();
        assert!(rendered.contains("Address summary-only change-request review"));
        assert!(rendered.contains("Latest CHANGES_REQUESTED review:"));
        assert!(
            rendered.contains("alice @ 2026-05-15T12:34:56+00:00"),
            "missing witness label: {rendered}",
        );
        assert!(
            rendered.contains("> Please factor the duplicated parsing logic"),
            "missing first body line: {rendered}",
        );
        assert!(
            rendered.contains("> into a shared helper."),
            "missing second body line: {rendered}",
        );
        // Generalization preamble preserved.
        assert!(rendered.contains("think deeply about the entire class of issue"));
    }

    #[test]
    fn address_change_request_prompt_falls_back_when_no_human_review_observed() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        // latest_human_changes_requested intentionally None.
        let cs = candidates(&oriented_with(r));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
            .expect("AddressChangeRequest must fire");
        let rendered = action.rendered_payload();
        assert!(rendered.contains("No human CHANGES_REQUESTED review observed"));
        assert!(rendered.contains("gh pr view --json reviews"));
        assert!(rendered.contains("think deeply about the entire class of issue"));
    }

    #[test]
    fn address_change_request_fallback_uses_step_form_with_command_adjacent() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let cs = candidates(&oriented_with(r));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
            .expect("AddressChangeRequest must fire");
        let rendered = action.rendered_payload();
        let step1 = rendered.find("Step 1").expect("step 1 present");
        let command = rendered
            .find("gh pr view --json reviews")
            .expect("command present");
        let step2 = rendered.find("Step 2").expect("step 2 present");
        assert!(step1 < command, "command should follow Step 1 header");
        assert!(
            command < step2,
            "command should sit between Step 1 and Step 2, not after",
        );
    }

    #[test]
    fn address_threads_prompt_orders_resolve_command_after_step_2_header() {
        let r = clean_reviews();
        let threads = vec![live_thread("src/a.rs", 1, "x")];
        let cs = candidates(&oriented_with_threads(r, threads));
        let rendered = cs[0].rendered_payload();
        let step1 = rendered.find("Step 1").expect("step 1 present");
        let step2 = rendered.find("Step 2").expect("step 2 present");
        let resolve = rendered
            .find("resolveReviewThread")
            .expect("resolve mutation present");
        let idempotency = rendered
            .find("idempotent")
            .expect("idempotency note present");
        assert!(step1 < step2, "Step 1 must precede Step 2");
        assert!(step2 < resolve, "resolve command should follow Step 2");
        assert!(
            resolve < idempotency,
            "idempotency note should follow the command, not precede it",
        );
    }

    #[test]
    fn address_threads_outdated_uses_step_1a_label() {
        let r = clean_reviews();
        let threads = vec![outdated_thread("src/a.rs", 1, "stale", "T_o")];
        let cs = candidates(&oriented_with_threads(r, threads));
        let rendered = cs[0].rendered_payload();
        assert!(
            rendered.contains("Step 1a"),
            "outdated sub-step should be labeled Step 1a: {rendered}",
        );
    }
}
