//! Review-axis candidates.
//!
//! Three families: per-thread remediation (drives agent work on
//! unresolved feedback), per-reviewer wait (bot and human), and
//! decision-derived candidates that close the review loop
//! (approval request, summary-only change-request).

use crate::ids::{BlockerKey, Timestamp};

use crate::observe::github::pull_request_view::ReviewDecision;
use crate::orient::ci::CiReport;
use crate::orient::copilot::CopilotReport;
use crate::orient::reviews::{HumanReview, ReviewSummary};
use crate::orient::thread::{
    BotName, FilePath, ReviewThread, ThreadAuthor, ThreadId, ThreadLocation, ThreadState,
};

use super::action::{Action, ActionEffect, ActionKind, MidTier, NonEmpty, TargetEffect, Urgency};
use ooda_core::{HandoffPrompt, SingleLineString, Witness};

/// Comma-join a slice of any `Display` for human-readable rendering.
fn join_display<T: std::fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(T::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Declared deps: own review report + CI report (for
/// `ci_clean` gate) + bot-review-axis presence (for shadow
/// filter) + threads.
#[allow(clippy::too_many_lines)]
pub(crate) fn candidates(
    reviews: &ReviewSummary,
    ci: &CiReport,
    copilot: Option<&CopilotReport>,
    threads: &[ReviewThread],
) -> Vec<Action> {
    let ci = &ci.summary;
    let mut out: Vec<Action> = Vec::new();

    // Agent-addressable unresolved set: every unresolved thread
    // EXCEPT outdated-human-authored ones. Rationale:
    // - Live thread (human or bot): agent reads, fixes, resolves.
    // - Outdated bot thread: agent evaluates on merit (line anchor
    //   is stale but the semantic concern may still apply), fixes
    //   if it does, replies, resolves.
    // - Outdated human thread: agent does NOT unilaterally resolve.
    //   The original commenter may still care about the stale
    //   concern; only a human should clear it. These flow to
    //   `merge_eligibility::merge_blocked_by_threads` at
    //   `BlockingHuman`, which fires the human-handoff path.
    let mut unresolved_threads: Vec<ReviewThread> = threads
        .iter()
        .filter(|t| t.state != ThreadState::Resolved)
        .filter(|t| {
            !(t.state == ThreadState::Outdated && matches!(t.author, ThreadAuthor::Human(_)))
        })
        .cloned()
        .collect();

    // Copilot suppresses low-confidence findings from being posted as
    // inline review threads — it only renders them as text inside a
    // `<details>` block at the tail of the review body. The orient
    // axis extracts those entries onto `CopilotReviewRound`; synthesise
    // them as `ReviewThread` rows here so they flow through the same
    // `AddressThreads` action the agent already knows how to discharge.
    //
    // Staleness gate: only synthesise when `copilot.fresh` (latest
    // round's review-commit equals current HEAD). When the agent has
    // pushed fixes since the latest review, the suppressed list is
    // keyed to the OLD commit — its entries are likely stale, and
    // their synthetic IDs have no `resolveReviewThread` path so the
    // loop would surface phantom threads on every iteration. Wait
    // for Copilot's next review of HEAD before re-emitting.
    if let Some(copilot) = copilot
        && copilot.fresh
        && let Some(latest) = copilot.rounds.last()
    {
        unresolved_threads.extend(synthesise_suppressed_threads(latest));
    }

    if let Some(unresolved_threads) = NonEmpty::try_from_vec(unresolved_threads) {
        let prompt = address_threads_prompt(&unresolved_threads);
        out.push(Action {
            kind: ActionKind::AddressThreads {
                threads: unresolved_threads,
            },
            effect: ActionEffect::Agent { prompt },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            // Gate-stable: progress on cardinality must not mask
            // as stall. The witness travels on the action; the key
            // names the gate (one or more unresolved threads),
            // which is identical for live and outdated entries —
            // both demand per-thread agent judgment.
            blocker: BlockerKey::from_static("unresolved_threads"),
        });
    }

    // When the bot-review axis is active, ownership of that bot's
    // pending-state polling belongs to it — its timing-aware
    // classifier needs the dedicated wait shape. Filter the bot's
    // login from this axis's pending list so the two axes do not
    // emit colliding waits in the same tier and the more granular
    // one is not shadowed by axis-order tiebreaking. Generic bots
    // and bots-on-repos-without-the-axis are unaffected.
    let bots_filtered: Vec<_> = if copilot.is_some() {
        reviews
            .pending_reviews
            .bots
            .iter()
            .filter(|bot| !crate::orient::copilot::is_copilot(bot.as_str()))
            .cloned()
            .collect()
    } else {
        reviews.pending_reviews.bots.clone()
    };
    if let Some(bots) = NonEmpty::try_from_vec(bots_filtered) {
        let names = join_display(&bots);
        out.push(Action {
            kind: ActionKind::WaitForBotReview { reviewers: bots },
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(60),
                log: format!("Wait for bot review from {names}"),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingWait),
            // Gate identity: "≥1 pending bot review". Reviewer
            // identities travel on the payload.
            blocker: BlockerKey::from_static("pending_bot_review"),
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
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            // Gate identity: "≥1 pending human review". Reviewer
            // identities travel on the payload.
            blocker: BlockerKey::from_static("pending_human_review"),
        });
    }

    // Approval is the loop-closing candidate: it fires only when
    // the upstream decision asks for review and no other work is
    // in flight. A changes-requested decision is explicitly not
    // an approval-request situation — the requested changes must
    // be addressed before re-review.
    let needs_approval = matches!(reviews.decision, Some(ReviewDecision::ReviewRequired));
    // `missing_names` carries required checks that are configured
    // but not yet present on the PR — an approval request here
    // would race ahead of the gate.
    let ci_clean =
        ci.required.fail() == 0 && ci.required.pending() == 0 && ci.missing_names.is_empty();
    // Gate symmetry with the address-threads filter: an outdated
    // thread is still unresolved feedback (anchor moved, content
    // may still apply), so any approval-closing candidate must
    // wait for the resolved state on every thread.
    let threads_clean = !threads.iter().any(|t| t.state != ThreadState::Resolved);
    if needs_approval && ci_clean && threads_clean {
        out.push(Action {
            kind: ActionKind::RequestApproval,
            effect: ActionEffect::Human {
                prompt: request_approval_prompt(reviews),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("not_approved"),
        });
    }

    // Summary-only change request: a changes-requested decision
    // with no inline thread payload. Without this candidate, the
    // empty action set on a still-blocked PR would halt Success.
    //
    // Class invariant — *every blocking review decision must
    // produce a candidate*. Suppression on pending re-review is
    // the composition rule: a re-request is already outstanding,
    // so re-firing as an agent fix would shadow the more
    // appropriate wait at a higher urgency tier.
    let changes_requested = matches!(reviews.decision, Some(ReviewDecision::ChangesRequested));
    let no_pending_re_review =
        reviews.pending_reviews.bots.is_empty() && reviews.pending_reviews.humans.is_empty();
    if changes_requested && threads_clean && no_pending_re_review {
        let bot_change_request_observed = reviews
            .bot_reviews
            .iter()
            .any(|b| b.state == crate::observe::github::reviews::ReviewState::ChangesRequested);
        out.push(Action {
            kind: ActionKind::AddressChangeRequest,
            effect: ActionEffect::Agent {
                prompt: address_change_request_prompt(
                    reviews.latest_human_changes_requested.as_ref(),
                    bot_change_request_observed,
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("changes_requested_summary"),
        });
    }

    out
}

/// Build the summary-only-change-request prompt. When a human
/// changes-requested review is observed, inline its author,
/// timestamp, and body as a witness so the agent reads what was
/// asked without a round-trip to the upstream review surface.
///
/// Absent that observation, the prompt distinguishes two cases by
/// the `bot_change_request_observed` flag:
///
/// 1. A bot review carries the `ChangesRequested` state — the
///    summary-only decision came from a bot rather than a human.
///    Legitimate; the agent fetches the bot's review body and
///    addresses it.
/// 2. No `ChangesRequested` row in any observed review (human or
///    bot). The host's decision-feed and reviews-feed disagree;
///    typically a transient REST/GraphQL race but persistent
///    disagreement signals a broken reviews observation. The
///    prompt names that explicitly so a recurring case is
///    diagnosable rather than silently masked.
fn address_change_request_prompt(
    latest: Option<&HumanReview>,
    bot_change_request_observed: bool,
) -> HandoffPrompt {
    let mut prompt =
        HandoffPrompt::new("Address summary-only change-request review (no inline threads).");

    if let Some(h) = latest {
        let when = h
            .submitted_at
            .as_ref()
            .map_or_else(|| "unknown time".to_string(), Timestamp::to_string);
        let label = SingleLineString::new(format!("{} @ {when}", h.author));
        let body = if h.body.trim().is_empty() {
            "> (review body was empty)".to_string()
        } else {
            h.body
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        prompt.push_paragraph("Latest CHANGES_REQUESTED review:".to_string());
        prompt.push_witnesses(NonEmpty::singleton(Witness {
            label,
            body: body.into(),
            url: None,
        }));
    } else if bot_change_request_observed {
        prompt.push_paragraph(
            "The CHANGES_REQUESTED decision came from a bot review (no human \
             CHANGES_REQUESTED row observed in this snapshot). Fetch the bot \
             review body and address it.",
        );
        prompt.push_heading(3, "Step 1 — fetch the bot CHANGES_REQUESTED review body");
        prompt.push_code("bash", "gh pr view --json reviews");
        prompt.push_heading(3, "Step 2 — address the requested changes");
    } else {
        // Neither a human nor a bot CHANGES_REQUESTED row in the
        // reviews projection. The host's decision feed and reviews
        // feed disagree on what's gating merge — most often a
        // transient race; persistent disagreement indicates a
        // broken reviews observation rather than a missing review.
        prompt.push_paragraph(
            "The host reports decision=CHANGES_REQUESTED but no matching review \
             row (human or bot) was observed. The decision feed and reviews \
             feed disagree — usually a transient race that resolves on the \
             next iteration. If this iteration is one of several with the \
             same disagreement, the reviews observation may be broken \
             (pagination loss, endpoint failure, or projection gap).",
        );
        prompt.push_heading(3, "Step 1 — fetch the latest CHANGES_REQUESTED review body");
        prompt.push_code("bash", "gh pr view --json reviews");
        prompt.push_heading(3, "Step 2 — address the requested changes");
    }

    prompt.push_paragraph(
        "For each issue, think deeply about the entire class of issue, in \
         general, and solve the general form of the issue across all relevant \
         code. This ensures the entire category of each issue is solved in \
         general.",
    );

    prompt
}

/// Build the approval-request prompt. Surfaces the required-reviewer
/// list and the current approval ratio at HEAD so the human knows
/// which signature is missing. When the required-reviewer list is
/// empty (upstream has not yet fanned in code-owner or branch-rule
/// requirements) the headline alone is sufficient.
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

/// Build the address-threads prompt, inlining each thread as a
/// witness so the actor's input is self-contained — no round-trip
/// to the upstream review surface to discover what to address.
/// Sections compose around the thread payload: headline (with a
/// live/outdated breakdown when relevant), per-author tally,
/// witnesses, class-of-issue generalization directive, optional
/// outdated-judgment directive, and the resolve-template with
/// idempotency note.
/// Synthesise `ReviewThread` rows from a Copilot round's suppressed
/// low-confidence entries. The host posts these as text only and
/// never assigns them stable node ids; this layer mints synthetic
/// ids that are deterministic within a (PR, SHA, path, line) tuple so
/// the stall classifier's gate identity stays stable across iterations.
/// Entries whose path is not a valid `FilePath` (control bytes, leading
/// slash, etc.) are dropped at this boundary rather than propagated.
fn synthesise_suppressed_threads(
    round: &crate::orient::copilot::CopilotReviewRound,
) -> Vec<ReviewThread> {
    let Some(reviewed_at) = round.reviewed_at else {
        return Vec::new();
    };
    let sha_prefix = round
        .commit
        .as_ref()
        .map(|c| c.as_str().chars().take(7).collect::<String>())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(round.suppressed_comments.len());
    for c in &round.suppressed_comments {
        let Ok(path) = FilePath::new(c.path.clone()) else {
            continue;
        };
        let id_raw = format!("copilot-suppressed:{sha_prefix}:{}:{}", c.path, c.line);
        let Ok(id) = ThreadId::new(id_raw) else {
            continue;
        };
        out.push(ReviewThread {
            id,
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path,
                line: Some(c.line),
            },
            body: c.body.clone(),
            // Live — actionable. The host never marks these resolved;
            // the gate stays open until the agent pushes a fix that
            // removes the underlying issue, after which Copilot's next
            // review (if any) renders a smaller `<details>` block.
            state: ThreadState::Live,
            originating_comment_id: None,
            created_at: reviewed_at,
        });
    }
    out
}

#[allow(clippy::too_many_lines)]
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
            .map(|(author, count)| {
                format!("**{}:** {}", author, crate::text::count(*count, "issue"))
            })
            .collect();
        prompt.push_paragraph(format!("{}.", bits.join(" · ")));
    }

    // `enumerate_map_ref` carries the non-empty invariant through
    // the map structurally; no runtime cardinality check is
    // needed at the output boundary.
    let witnesses = threads.enumerate_map_ref(|i, t| {
        let tag = match t.state {
            ThreadState::Outdated => "    [outdated]",
            // Live renders unadorned; Resolved cannot reach here
            // because the caller filters it out.
            _ => "",
        };
        let comment_id_field = t
            .originating_comment_id
            .map(|id| format!("    comment_id: {id}"))
            .unwrap_or_default();
        let label = SingleLineString::new(format!(
            "{}. {} @ {}{}    thread_id: {}{}",
            i + 1,
            t.author,
            t.location,
            tag,
            t.id,
            comment_id_field,
        ));
        let body = t
            .body
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        Witness {
            label,
            body: body.into(),
            url: None,
        }
    });
    prompt.push_witnesses(witnesses);

    prompt.push_heading(3, "Step 1 — solve the class, not the instance");
    prompt.push_paragraph(
        "For each issue, think deeply about the entire class of issue, in \
         general, and solve the general form of the issue across all relevant \
         code. This ensures the entire category of each issue is solved in \
         general, not just the specific instance the reviewer flagged.",
    );

    if outdated_count > 0 {
        prompt.push_heading(3, "Step 1a — outdated threads only");
        prompt.push_paragraph(
            "GitHub's `isOutdated` flag is positional, not content-relevance: \
             the diff hunk that anchored the thread has moved (typically due \
             to a refactor or rebase), so the comment no longer renders \
             inline, but the logical feedback may still apply to the current \
             code. For each outdated thread, locate the current code that the \
             comment is about (often near the original `path:line` after a \
             small refactor; sometimes elsewhere) and decide whether the \
             feedback still applies. If it does, address it as you would a \
             live thread. If it does not, post a brief reply explaining why.",
        );
        prompt.push_paragraph(
            "**Reply path for outdated / null-line threads** — the \
             line-anchored suggestion API is unavailable, but a thread reply \
             via the comment-replies endpoint works. The endpoint is keyed \
             by the originating comment's REST `comment_id` (shown in each \
             entry above when known), not by line:",
        );
        prompt.push_code(
            "bash",
            "gh api repos/<owner>/<repo>/pulls/<pr>/comments/<comment_id>/replies \
             -f body='<your reply text>'",
        );
    }

    prompt.push_heading(3, "Step 2 — mark each thread resolved");
    prompt.push_paragraph(
        "After addressing (or judging not-applicable) each thread, mark it \
         resolved on GitHub by running:",
    );
    prompt.push_code(
        "bash",
        "gh api graphql -f query='mutation { resolveReviewThread(input: \
         { threadId: \"<thread_id>\" }) { thread { id } } }'",
    );
    prompt.push_paragraph(
        "Substitute the per-thread `thread_id` shown in each entry above. The \
         mutation is idempotent — already-resolved threads succeed as a no-op.",
    );

    prompt
}

/// Tally by author, preserving first-seen order. Linear scan is
/// sufficient for the realistic per-PR scale and avoids requiring
/// hashability or ordering on the author sum.
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
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};

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

    /// Test helper: invoke `candidates` with default CI / no copilot / no threads.
    fn cands_with(reviews: &ReviewSummary) -> Vec<Action> {
        cands_with_threads(reviews, &[])
    }

    fn cands_with_threads(reviews: &ReviewSummary, threads: &[ReviewThread]) -> Vec<Action> {
        candidates(reviews, &clean_ci(), None, threads)
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
            id: ThreadId::new(id.to_string()).unwrap(),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new(path).unwrap(),
                line: Some(line),
            },
            body: body.into(),
            state,
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            originating_comment_id: None,
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
        assert!(cands_with(&clean_reviews()).is_empty());
    }

    /// Build a `CopilotReport` whose latest round carries `suppressed_comments`
    /// and nothing else interesting. Reuses the testing helpers from this
    /// module's own test scope.
    fn copilot_report_with_suppressed(
        entries: Vec<crate::orient::copilot::SuppressedComment>,
    ) -> crate::orient::copilot::CopilotReport {
        use crate::ids::GitCommitSha;
        use crate::orient::bot_threads::BotThreadSummary;
        use crate::orient::copilot::{
            CopilotActivity, CopilotRepoConfig, CopilotReport, CopilotReviewRound, CopilotTier,
        };
        let round = CopilotReviewRound {
            round: 1,
            requested_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            ack_at: Some(Timestamp::parse("2026-04-23T10:01:00Z").unwrap()),
            reviewed_at: Some(Timestamp::parse("2026-04-23T10:05:00Z").unwrap()),
            commit: Some(GitCommitSha::parse(&"a".repeat(40)).unwrap()),
            comments_visible: 0,
            #[allow(clippy::cast_possible_truncation)]
            comments_suppressed: entries.len() as u32,
            suppressed_comments: entries,
        };
        CopilotReport {
            config: CopilotRepoConfig {
                enabled: true,
                review_on_push: false,
                review_draft_pull_requests: false,
            },
            activity: CopilotActivity::Reviewed {
                latest: round.clone(),
            },
            rounds: vec![round],
            threads: BotThreadSummary::default(),
            tier: CopilotTier::Silver,
            fresh: true,
        }
    }

    #[test]
    fn suppressed_low_confidence_entries_fire_address_threads_action() {
        use crate::orient::copilot::SuppressedComment;
        let report = copilot_report_with_suppressed(vec![
            SuppressedComment {
                path: "src/lib.rs".into(),
                line: 12,
                body: "stale doc comment".into(),
            },
            SuppressedComment {
                path: "src/lib.rs".into(),
                line: 30,
                body: "name drift".into(),
            },
        ]);
        let cs = candidates(&clean_reviews(), &clean_ci(), Some(&report), &[]);
        let address = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }));
        let address = address.expect("AddressThreads must fire when suppressed entries exist");
        match &address.kind {
            ActionKind::AddressThreads { threads } => {
                assert_eq!(threads.len(), 2);
                assert!(threads.iter().any(|t| t.location.line == Some(12)));
                assert!(threads.iter().any(|t| t.location.line == Some(30)));
                assert!(threads.iter().all(|t| matches!(t.state, ThreadState::Live)));
                assert!(
                    threads
                        .iter()
                        .all(|t| matches!(t.author, ThreadAuthor::Bot(_)))
                );
            }
            other => panic!("unexpected kind {other:?}"),
        }
    }

    #[test]
    fn suppressed_entries_not_synthesised_when_copilot_not_fresh() {
        // Staleness gate: when the latest Copilot round was on a
        // commit older than HEAD (`copilot.fresh = false`), the
        // suppressed-comment list is keyed to the stale commit.
        // Its entries may have been fixed already; synthetic IDs
        // have no `resolveReviewThread` path, so emitting them
        // would trap the loop on phantom AddressThreads. Wait for
        // Copilot's next review of HEAD before re-emitting.
        use crate::orient::copilot::SuppressedComment;
        let mut report = copilot_report_with_suppressed(vec![SuppressedComment {
            path: "src/lib.rs".into(),
            line: 12,
            body: "stale doc comment".into(),
        }]);
        report.fresh = false;
        let cs = candidates(&clean_reviews(), &clean_ci(), Some(&report), &[]);
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. })),
            "AddressThreads must NOT fire on stale Copilot round; got {cs:?}"
        );
    }

    #[test]
    fn suppressed_entries_with_invalid_path_are_dropped_silently() {
        use crate::orient::copilot::SuppressedComment;
        // Leading `/` fails FilePath::new; entry is dropped, not raised.
        let report = copilot_report_with_suppressed(vec![SuppressedComment {
            path: "/abs/path.rs".into(),
            line: 1,
            body: "this one is unreachable".into(),
        }]);
        let cs = candidates(&clean_reviews(), &clean_ci(), Some(&report), &[]);
        assert!(
            cs.iter()
                .all(|a| !matches!(a.kind, ActionKind::AddressThreads { .. })),
            "no AddressThreads when every suppressed entry is invalid",
        );
    }

    #[test]
    fn suppressed_entries_merge_with_inline_threads_into_single_action() {
        use crate::orient::copilot::SuppressedComment;
        let report = copilot_report_with_suppressed(vec![SuppressedComment {
            path: "src/lib.rs".into(),
            line: 42,
            body: "from suppressed block".into(),
        }]);
        let inline = vec![live_thread("src/other.rs", 5, "from real review thread")];
        let cs = candidates(&clean_reviews(), &clean_ci(), Some(&report), &inline);
        match &cs[0].kind {
            ActionKind::AddressThreads { threads } => {
                assert_eq!(
                    threads.len(),
                    2,
                    "suppressed + inline thread merge into one payload",
                );
            }
            other => panic!("unexpected kind {other:?}"),
        }
    }

    #[test]
    fn unresolved_threads_emit_address_threads() {
        let r = clean_reviews();
        let threads = vec![
            live_thread("src/a.rs", 1, "first"),
            live_thread("src/b.rs", 2, "second"),
            live_thread("src/c.rs", 3, "third"),
        ];
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with_threads(&r, &threads);
        let desc = &cs[0].rendered_payload();
        // Headline + per-author breakdown
        assert!(desc.contains("Address 2 unresolved review threads."));
        assert!(desc.contains("**Copilot:** 2 issues."));
        // Both witnesses inlined (location + body)
        assert!(desc.contains("Copilot @ src/foo.rs:42"));
        assert!(desc.contains("> unwrap should be ?"));
        assert!(desc.contains("Copilot @ src/bar.rs:99"));
        assert!(desc.contains("> missing error context"));
        // Generalization preamble preserved
        assert!(desc.contains("think deeply about the entire class of issue"));
    }

    #[test]
    fn address_threads_prompt_includes_comment_id_when_present() {
        // When the orient layer surfaces the originating comment's
        // REST databaseId, the witness label carries `comment_id:`
        // so the agent can hit the line-anchored-replies endpoint
        // directly without a round-trip GraphQL fetch.
        let r = clean_reviews();
        let mut t = outdated_thread("src/foo.rs", 42, "review body", "T_outdated");
        t.originating_comment_id = Some(3_377_501_272);
        let cs = cands_with_threads(&r, &[t]);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads fires");
        let rendered = action.rendered_payload();
        assert!(
            rendered.contains("comment_id: 3377501272"),
            "comment_id must appear in the witness label"
        );
        // Outdated branch also surfaces the replies endpoint recipe.
        assert!(rendered.contains("comments/<comment_id>/replies"));
    }

    #[test]
    fn address_threads_prompt_omits_comment_id_when_absent() {
        // Defensive fallback when the wire doesn't carry databaseId
        // (older fixtures): the witness label still renders, just
        // without the comment_id field. The reply-path recipe still
        // appears because the thread is outdated.
        let r = clean_reviews();
        let t = outdated_thread("src/foo.rs", 42, "review body", "T_outdated");
        // originating_comment_id defaults to None via the helper.
        let cs = cands_with_threads(&r, &[t]);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads fires");
        let rendered = action.rendered_payload();
        assert!(!rendered.contains("comment_id:"));
    }

    #[test]
    fn outdated_bot_threads_emit_address_threads() {
        // The upstream's outdated marker is positional only; the
        // content may still apply. For bot-authored outdated
        // threads, the agent can evaluate on merit, fix if the
        // concern still applies, reply via the thread-comment
        // surface, and resolve via GraphQL — no human verdict
        // required. AddressThreads fires.
        let r = clean_reviews();
        let threads = vec![outdated_thread(
            "src/foo.rs",
            42,
            "still wrong even after move",
            "T_outdated",
        )];
        let cs = cands_with_threads(&r, &threads);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads must fire on outdated bot threads");
        assert!(matches!(action.effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn outdated_human_threads_are_excluded_from_address_threads() {
        // Outdated human-authored threads are NOT agent-
        // addressable: the agent should not unilaterally resolve
        // a human's stale concern. The merge_eligibility axis
        // handles them via `merge_blocked_threads` at
        // BlockingHuman.
        let r = clean_reviews();
        let mut human_thread = outdated_thread("src/foo.rs", 42, "your concern", "T_human");
        human_thread.author = ThreadAuthor::Human(crate::ids::GitHubLogin::parse("alice").unwrap());
        let cs = cands_with_threads(&r, &[human_thread]);
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. })),
            "AddressThreads must NOT fire on outdated human threads; got {cs:?}"
        );
    }

    #[test]
    fn outdated_botname_other_included_in_address_threads() {
        // The filter matches `ThreadAuthor::Bot(_)` — every BotName
        // variant flows through, not just Copilot/Cursor. Verifies
        // a graphite-app bot is included alongside the modeled
        // two.
        use crate::orient::thread::BotName;
        let r = clean_reviews();
        let mut t = outdated_thread("src/foo.rs", 1, "graphite says X", "T_graphite");
        t.author = ThreadAuthor::Bot(BotName::Other(GitHubLogin::parse("graphite-app").unwrap()));
        let cs = cands_with_threads(&r, &[t]);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("graphite-app outdated must flow through AddressThreads");
        let ActionKind::AddressThreads { threads } = &action.kind else {
            panic!("expected AddressThreads");
        };
        assert_eq!(threads.len(), 1);
    }

    #[test]
    fn live_human_plus_outdated_human_only_live_in_address_threads() {
        // Live-human flows; outdated-human is filtered. The
        // resulting AddressThreads payload should contain only
        // the live-human entry — the outdated-human waits for
        // merge_eligibility's BlockingHuman path.
        let r = clean_reviews();
        let mut live = live_thread("src/a.rs", 1, "live concern");
        live.author = ThreadAuthor::Human(GitHubLogin::parse("alice").unwrap());
        let mut outdated_h = outdated_thread("src/b.rs", 2, "stale concern", "T_outdated");
        outdated_h.author = ThreadAuthor::Human(GitHubLogin::parse("alice").unwrap());
        let cs = cands_with_threads(&r, &[live, outdated_h]);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads fires for the live thread");
        let ActionKind::AddressThreads { threads } = &action.kind else {
            panic!("expected AddressThreads");
        };
        assert_eq!(threads.len(), 1);
        assert!(
            matches!(threads.first().author, ThreadAuthor::Human(_))
                && threads.first().state == ThreadState::Live
        );
    }

    #[test]
    fn outdated_human_excluded_but_outdated_bot_included_in_mixed() {
        // Mixed outdated set: AddressThreads fires with the bot
        // entry only. The human's stale concern flows to
        // merge_eligibility's human-handoff path.
        let r = clean_reviews();
        let mut human_thread = outdated_thread("src/a.rs", 1, "humans stale", "T_human");
        human_thread.author = ThreadAuthor::Human(crate::ids::GitHubLogin::parse("alice").unwrap());
        let bot_thread = outdated_thread("src/b.rs", 2, "bot stale", "T_bot");
        let cs = cands_with_threads(&r, &[human_thread, bot_thread]);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressThreads { .. }))
            .expect("AddressThreads fires");
        let ActionKind::AddressThreads { threads } = &action.kind else {
            panic!("expected AddressThreads");
        };
        assert_eq!(threads.len(), 1, "only the bot thread should flow through");
        assert!(matches!(threads.first().author, ThreadAuthor::Bot(_)));
    }

    #[test]
    fn mixed_live_and_outdated_threads_share_one_action() {
        let r = clean_reviews();
        let threads = vec![
            live_thread("src/a.rs", 1, "live concern"),
            outdated_thread("src/b.rs", 2, "outdated concern", "T_outdated"),
        ];
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with_threads(&r, &threads);
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressThreads { .. })),
            "All-Resolved set must not emit AddressThreads"
        );
    }

    fn copilot_active_report() -> crate::orient::copilot::CopilotReport {
        use crate::orient::copilot::{
            CopilotActivity, CopilotRepoConfig, CopilotReport, CopilotTier, InFlightHealth,
        };
        CopilotReport {
            config: CopilotRepoConfig {
                enabled: true,
                review_on_push: false,
                review_draft_pull_requests: false,
            },
            activity: CopilotActivity::Requested {
                requested_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                health: InFlightHealth::Healthy,
            },
            rounds: vec![],
            threads: crate::orient::bot_threads::BotThreadSummary::default(),
            tier: CopilotTier::Bronze,
            fresh: false,
        }
    }

    fn cands_with_copilot(reviews: &ReviewSummary, copilot: Option<&CopilotReport>) -> Vec<Action> {
        candidates(reviews, &clean_ci(), copilot, &[])
    }

    #[test]
    fn pending_bots_emit_wait_for_bot_review_without_copilot_axis() {
        // Baseline: no copilot axis active → Copilot in pending_bots
        // flows through unfiltered.
        let mut r = clean_reviews();
        r.pending_reviews.bots =
            vec![GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap()];
        let cs = cands_with_copilot(&r, None);
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::WaitForBotReview { .. })),
            "expected WaitForBotReview emission when copilot axis is absent: {:?}",
            cs.iter().map(|a| &a.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn copilot_filtered_from_wait_for_bot_review_when_copilot_axis_active() {
        // Copilot axis active → reviews-axis must not shadow it; the
        // pending Copilot login is filtered out, no WaitForBotReview
        // emission (because Copilot was the only pending bot).
        let mut r = clean_reviews();
        r.pending_reviews.bots =
            vec![GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap()];
        let cs = cands_with_copilot(&r, Some(&copilot_active_report()));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::WaitForBotReview { .. })),
            "expected NO WaitForBotReview when copilot axis owns Copilot: {:?}",
            cs.iter().map(|a| &a.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn non_copilot_bots_still_emit_wait_for_bot_review_when_copilot_axis_active() {
        // Copilot filtered, but a non-Copilot bot (e.g. Renovate) is
        // still emitted — reviews-axis remains the home for generic
        // bot reviewers.
        let mut r = clean_reviews();
        r.pending_reviews.bots = vec![
            GitHubLogin::parse("copilot-pull-request-reviewer[bot]").unwrap(),
            GitHubLogin::parse("renovate[bot]").unwrap(),
        ];
        let cs = cands_with_copilot(&r, Some(&copilot_active_report()));
        let wait_for_bot = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::WaitForBotReview { .. }))
            .expect("expected WaitForBotReview for the non-Copilot bot");
        let ActionKind::WaitForBotReview { reviewers } = &wait_for_bot.kind else {
            unreachable!()
        };
        let logins: Vec<&str> = reviewers.iter().map(GitHubLogin::as_str).collect();
        assert_eq!(
            logins,
            vec!["renovate[bot]"],
            "expected Copilot filtered out, Renovate retained"
        );
    }

    #[test]
    fn pending_humans_marked_human_automation() {
        let mut r = clean_reviews();
        r.pending_reviews.humans = vec![Reviewer::User(GitHubLogin::parse("alice").unwrap())];
        let cs = cands_with(&r);
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
        let cs = cands_with(&r.clone());
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );

        let threads = vec![live_thread("src/a.rs", 1, "x")];
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with_threads(&r, &threads);
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );
    }

    #[test]
    fn no_approval_when_decision_is_none() {
        let mut r = clean_reviews();
        r.decision = None;
        let cs = cands_with(&r);
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::RequestApproval))
        );
    }

    #[test]
    fn summary_only_change_request_emits_address_change_request() {
        // Witness for the class invariant: a blocking review
        // decision with no thread payload must still produce a
        // candidate, or the loop would halt Success on a still-
        // blocked PR.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.threads_unresolved = 0;
        let cs = cands_with(&r);
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
        // Suppression rule: the per-thread candidate already
        // covers the work; the summary-only candidate must not
        // emit a redundant gate.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let threads = vec![
            live_thread("src/a.rs", 1, "x"),
            live_thread("src/b.rs", 2, "y"),
        ];
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with(&r);
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
        );
    }

    #[test]
    fn change_request_fires_even_when_ci_failing() {
        // CI is orthogonal to the review-decision invariant; the
        // summary-only candidate fires on the review axis alone.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let mut ci = clean_ci();
        ci.summary.required.failed = vec![crate::orient::ci::FailedCheck {
            name: crate::ids::CheckName::parse("Lint").unwrap(),
            description: String::new(),
            link: String::new(),
        }];
        let cs = candidates(&r, &ci, None, &[]);
        assert!(
            cs.iter()
                .any(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
        );
    }

    #[test]
    fn change_request_suppressed_when_re_review_pending() {
        // Composition rule: an outstanding re-review wait at a
        // higher urgency tier covers the gate; re-firing as an
        // agent fix would send work back unnecessarily.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.pending_reviews.humans = vec![Reviewer::User(GitHubLogin::parse("alice").unwrap())];
        let cs = cands_with(&r);
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

    // ─── decision-coverage property ───────────────────────────────
    //
    // Pins the class invariant: every blocking review decision
    // produces exactly one decision-derived candidate in the
    // baseline configuration; non-blocking decisions produce none.
    // The exhaustive match below is the contract; a new variant
    // fails to compile until handled.

    /// Decision-derived axis behaviour in the baseline. Pending-
    /// reviewer waits are not decision-driven and are excluded by
    /// this property test's setup.
    #[derive(Debug, PartialEq, Eq)]
    enum ReviewAxisBehavior {
        /// Decision is non-blocking (none or already approved).
        NoBlocker,
        /// Approval-closing candidate fires.
        EmitRequestApproval,
        /// Summary-only-change-request candidate fires.
        EmitAddressChangeRequest,
    }

    /// Exhaustive contract over `Option<ReviewDecision>`. The
    /// `Unknown` arm collapses to no-blocker because the reviews
    /// axis has no actionable response to an unmodeled verdict;
    /// downstream surfaces still render the state (see
    /// `comment/render.rs::reviews_line`).
    fn expected_review_axis_behavior(decision: Option<ReviewDecision>) -> ReviewAxisBehavior {
        // Arms duplicated for spec clarity.
        #[allow(clippy::match_same_arms)]
        match decision {
            None => ReviewAxisBehavior::NoBlocker,
            Some(ReviewDecision::Approved) => ReviewAxisBehavior::NoBlocker,
            Some(ReviewDecision::ReviewRequired) => ReviewAxisBehavior::EmitRequestApproval,
            Some(ReviewDecision::ChangesRequested) => ReviewAxisBehavior::EmitAddressChangeRequest,
            Some(ReviewDecision::Unknown) => ReviewAxisBehavior::NoBlocker,
        }
    }

    fn all_review_decisions() -> Vec<Option<ReviewDecision>> {
        vec![
            None,
            Some(ReviewDecision::Approved),
            Some(ReviewDecision::ReviewRequired),
            Some(ReviewDecision::ChangesRequested),
            Some(ReviewDecision::Unknown),
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
            5,
            "Sample enumeration must cover `None` plus every decision \
             variant. A new variant requires both a sample here and an \
             arm in the exhaustive contract above.",
        );
        for decision in decisions {
            let mut r = clean_reviews();
            r.decision = decision;
            let cs = cands_with(&r);
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
        let cs = cands_with(&r);
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
        let cs = cands_with(&r);
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
        let cs = cands_with(&r);
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
        let cs = cands_with(&r);
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
    fn address_change_request_prompt_names_feed_disagreement_when_no_review_observed() {
        // Decision says CHANGES_REQUESTED, neither a human review
        // nor a bot review carries the matching state. The prompt
        // explicitly names the feed disagreement so persistent
        // recurrence is diagnosable rather than silently masked.
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let cs = cands_with(&r);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
            .expect("AddressChangeRequest must fire");
        let rendered = action.rendered_payload();
        assert!(rendered.contains("decision feed and reviews feed disagree"));
        assert!(rendered.contains("reviews observation may be broken"));
        assert!(rendered.contains("gh pr view --json reviews"));
        assert!(rendered.contains("think deeply about the entire class of issue"));
    }

    #[test]
    fn address_change_request_prompt_names_bot_path_when_bot_review_observed() {
        // Decision says CHANGES_REQUESTED, no human review observed,
        // but a bot review carries the matching state. The prompt
        // names this as the bot-only path — distinct from the
        // feed-disagreement case — so the agent fetches the bot's
        // body rather than chasing a phantom human review.
        use crate::observe::github::reviews::ReviewState;
        use crate::orient::reviews::BotReview;
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        r.bot_reviews = vec![BotReview {
            user: GitHubLogin::parse("review-bot").unwrap(),
            state: ReviewState::ChangesRequested,
            submitted_at: None,
        }];
        let cs = cands_with(&r);
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::AddressChangeRequest))
            .expect("AddressChangeRequest must fire");
        let rendered = action.rendered_payload();
        assert!(rendered.contains("came from a bot review"));
        assert!(
            !rendered.contains("decision feed and reviews feed disagree"),
            "must NOT name feed disagreement when a bot review explains the decision"
        );
        assert!(rendered.contains("gh pr view --json reviews"));
    }

    #[test]
    fn address_change_request_fallback_uses_step_form_with_command_adjacent() {
        let mut r = clean_reviews();
        r.decision = Some(ReviewDecision::ChangesRequested);
        let cs = cands_with(&r);
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
        let cs = cands_with_threads(&r, &threads);
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
        let cs = cands_with_threads(&r, &threads);
        let rendered = cs[0].rendered_payload();
        assert!(
            rendered.contains("Step 1a"),
            "outdated sub-step should be labeled Step 1a: {rendered}",
        );
    }
}
