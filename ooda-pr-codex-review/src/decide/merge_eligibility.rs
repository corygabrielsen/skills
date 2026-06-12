//! Merge-eligibility closure check.
//!
//! Asserts that "if no axis-emitted candidate explains the merge
//! state" coincides with "GitHub will merge this PR right now."
//! Without this assertion, OODA's verdict-by-absence would project
//! a still-unmergeable PR into `Decision::Halt(Success)` whenever
//! the explaining gate sits outside the modeled axis set or is
//! masked by a transient signal.
//!
//! # Invariants
//!
//! - **No masking by other axes**: this axis runs unconditionally.
//!   It does not gate on `has_advancement_path` because BLOCKED is
//!   never "explained away" by a coincidentally-firing axis on a
//!   different concern.
//! - **At most one candidate per call**: the drill-down picks the
//!   first matching cause in priority order so the prompt names
//!   one gate, not a list. Multiple causes manifest across
//!   iterations as the higher-priority one clears.
//! - **Outdated-unresolved threads count for merge-gating**:
//!   GitHub's conversation-resolution gate counts every non-resolved
//!   thread, including outdated ones. This module counts them all
//!   when computing `unresolved_total`. The reviews axis's
//!   `is_outdated` filter for `AddressThreads` is narrower: it
//!   excludes outdated **human-authored** threads (an agent
//!   shouldn't unilaterally resolve a human's stale concern) but
//!   includes outdated **bot-authored** threads (the bot's
//!   semantic feedback may still apply post-edit; the agent
//!   evaluates on merit, fixes if applicable, replies via the
//!   thread-comment surface, and resolves via the GraphQL
//!   `resolveReviewThread` mutation).
//! - **Stale-state cross-checks on green status**: when
//!   `mergeStateStatus ∈ {CLEAN, UNSTABLE, HAS_HOOKS}` the host
//!   says go, but the cross-checks for conversation-resolution and
//!   check-rollup are re-verified independently because BLOCKED ↔
//!   CLEAN transitions lag in known real bugs.

use crate::ids::BlockerKey;
use crate::observe::github::pull_request_view::{MergeStateStatus, ReviewDecision};
use crate::orient::ci::{CiActivity, CiReport, ResolvedState};
use crate::orient::reviews::ReviewSummary;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::{ReviewThread, ThreadAuthor, ThreadState};

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

/// Required-checks rollup state derived from the CI report. Distinct
/// from `CiActivity` which carries more shape than this axis needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequiredChecksRollup {
    Clean,
    Failure,
    Pending,
}

fn required_checks_rollup(ci: &CiReport) -> RequiredChecksRollup {
    if !ci.summary.required.failed.is_empty() {
        RequiredChecksRollup::Failure
    } else if !ci.summary.required.pending_names.is_empty() {
        RequiredChecksRollup::Pending
    } else {
        RequiredChecksRollup::Clean
    }
}

/// Compute the merge-eligibility candidate set.
///
/// Returns at most one candidate. The empty result attests "host
/// says merge is OK and our independent cross-checks agree."
pub(crate) fn merge_eligibility_candidates(
    state: &PullRequestProjection,
    threads: &[ReviewThread],
    reviews: &ReviewSummary,
    ci: &CiReport,
) -> Vec<Action> {
    let unresolved_total = threads
        .iter()
        .filter(|t| t.state != ThreadState::Resolved)
        .count();
    let rollup = required_checks_rollup(ci);

    match state.merge_state_status {
        MergeStateStatus::Unknown => vec![wait_merge_state_unknown()],
        MergeStateStatus::HasHooks => vec![wait_merge_state_has_hooks()],
        MergeStateStatus::Blocked => drill_blocked(
            state,
            threads,
            unresolved_total,
            reviews,
            rollup,
            &ci.activity,
        )
        .map_or(vec![], |a| vec![a]),
        MergeStateStatus::Clean | MergeStateStatus::Unstable => {
            cross_check_eligible(state, unresolved_total, rollup)
        }
        // Behind / Draft / Dirty are owned by the state axis; this
        // module emits nothing so the state axis's prompt is the
        // sole surface for the user.
        MergeStateStatus::Behind | MergeStateStatus::Dirty | MergeStateStatus::Draft => vec![],
    }
}

/// Drill `mergeStateStatus = Blocked` into the most likely cause.
/// First match wins; ordering reflects "most actionable-and-named
/// upstream gate first."
///
/// Returns `None` in two suppression cases where the agent path
/// covers the same threads/checks at a more specific tier:
///
/// 1. **Outdated-bot-only thread suppression**: every unresolved
///    thread is `Outdated` and bot-authored. The reviews axis
///    emits `AddressThreads` at `BlockingFix` for the same set;
///    this module stays silent so the agent path takes the
///    iteration rather than `HandoffHuman`.
///
/// 2. **Fan-in suppression**: rollup is `Clean` AND the CI
///    projection carries a healthy in-flight workflow for a
///    missing required check. `ci.rs`'s `WaitForCi(ci_missing)`
///    at `BlockingWait` takes the iteration.
///
/// All other paths return `Some(action)`.
fn drill_blocked(
    state: &PullRequestProjection,
    threads: &[ReviewThread],
    unresolved_total: usize,
    reviews: &ReviewSummary,
    rollup: RequiredChecksRollup,
    ci_activity: &CiActivity,
) -> Option<Action> {
    if state.conversation_resolution_required && unresolved_total > 0 {
        // Outdated-bot-only suppression: when every unresolved
        // thread is `Outdated` and bot-authored, the reviews axis
        // covers them with `AddressThreads` at `BlockingFix`.
        // Suppressing this HandoffHuman lets the agent path win
        // the iteration without tier inversion.
        let all_outdated_bot = threads
            .iter()
            .filter(|t| t.state != ThreadState::Resolved)
            .all(|t| t.state == ThreadState::Outdated && matches!(t.author, ThreadAuthor::Bot(_)));
        if all_outdated_bot {
            return None;
        }
        return Some(merge_blocked_by_threads(unresolved_total));
    }
    if matches!(
        reviews.decision,
        Some(ReviewDecision::ReviewRequired | ReviewDecision::ChangesRequested)
    ) {
        return Some(merge_blocked_by_review(reviews.decision));
    }
    // Ruleset-required approving review count not met on HEAD.
    if let Some(required) = state.required_approving_review_count
        && required > 0
        && u32::try_from(reviews.approvals_on_head).unwrap_or(u32::MAX) < required
    {
        return Some(merge_blocked_pending_approval(
            required,
            reviews.approvals_on_head,
        ));
    }
    match rollup {
        RequiredChecksRollup::Failure => Some(merge_blocked_by_check_failure()),
        RequiredChecksRollup::Pending => Some(merge_blocked_by_check_pending()),
        RequiredChecksRollup::Clean => {
            // Fan-in suppression: when a required check is missing
            // AND a healthy in-flight workflow on HEAD exists, the
            // absence of the required check is pending — not an
            // unmodeled policy gate. Defer to the routine wait
            // path.
            if let CiActivity::Resolved(ResolvedState::MissingRequired {
                healthy_in_flight_runs,
                ..
            }) = ci_activity
                && !healthy_in_flight_runs.is_empty()
            {
                return None;
            }
            Some(merge_blocked_by_policy(state))
        }
    }
}

/// Cross-check the eligible branch (Clean / Unstable). Host says
/// the merge button is green, but our re-derived conversation-
/// resolution + check-rollup may disagree on stale state.
fn cross_check_eligible(
    state: &PullRequestProjection,
    unresolved_total: usize,
    rollup: RequiredChecksRollup,
) -> Vec<Action> {
    if state.conversation_resolution_required && unresolved_total > 0 {
        return vec![merge_stale_threads(unresolved_total)];
    }
    if !matches!(rollup, RequiredChecksRollup::Clean) {
        return vec![merge_stale_checks(rollup)];
    }
    vec![]
}

fn wait_merge_state_unknown() -> Action {
    Action {
        kind: ActionKind::WaitForMergeability,
        effect: ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(30),
            log: "GitHub is still computing merge state — wait and re-observe".into(),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingWait),
        blocker: BlockerKey::from_static("merge_state_unknown"),
    }
}

fn wait_merge_state_has_hooks() -> Action {
    Action {
        kind: ActionKind::WaitForMergeability,
        effect: ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(30),
            log: "Commit hooks are still running — wait and re-observe".into(),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingWait),
        blocker: BlockerKey::from_static("merge_state_has_hooks"),
    }
}

fn merge_blocked_by_threads(unresolved_total: usize) -> Action {
    use ooda_core::HandoffPrompt;
    let prompt = HandoffPrompt::new(format!(
        "GitHub merge blocked: branch policy requires every review \
         conversation to be resolved before merging. {unresolved_total} \
         thread(s) on this PR are still unresolved (this count includes \
         outdated threads — the line anchor went stale but the thread \
         is still gating). Address them on their merit (the semantic \
         concern may still apply even when the anchor is stale): the \
         reviews axis emits `AddressThreads` at `BlockingFix` for any \
         agent-addressable subset. This handoff fires only when the \
         remaining unresolved set requires a human (e.g., an outdated \
         human-authored thread). Resolve via the PR UI or via the \
         GraphQL `resolveReviewThread` mutation."
    ));
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("merge_blocked_threads"),
    }
}

fn merge_blocked_by_review(decision: Option<ReviewDecision>) -> Action {
    use ooda_core::HandoffPrompt;
    let detail = match decision {
        Some(ReviewDecision::ChangesRequested) => "changes requested",
        Some(ReviewDecision::ReviewRequired) => "review required",
        _ => "approval missing",
    };
    let prompt = HandoffPrompt::new(format!(
        "GitHub merge blocked by review policy ({detail}). The branch \
         requires at least one approving review on HEAD; the current \
         reviewDecision does not satisfy it. Request the necessary \
         review or address the requested changes."
    ));
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("merge_blocked_review"),
    }
}

fn merge_blocked_pending_approval(required: u32, current: usize) -> Action {
    use ooda_core::HandoffPrompt;
    let prompt = HandoffPrompt::new(format!(
        "GitHub merge blocked: branch ruleset requires {required} \
         approving review(s) on HEAD. Current count: {current}. No \
         human or bot has flipped a review from COMMENTED to APPROVED \
         on this HEAD. Request an approving review (or, if Copilot is \
         the configured approver, re-request Copilot and wait for it \
         to APPROVE rather than just COMMENT)."
    ));
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("merge_blocked_pending_approval"),
    }
}

fn merge_blocked_by_check_failure() -> Action {
    use ooda_core::HandoffPrompt;
    let prompt = HandoffPrompt::new(
        "GitHub merge blocked: one or more required checks have a \
         terminal failure on HEAD. Inspect the required-check rollup \
         (CI axis) for the failed contexts; remediation lives in those \
         pipelines, not at the OODA layer.",
    );
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("merge_blocked_check_failure"),
    }
}

fn merge_blocked_by_check_pending() -> Action {
    Action {
        kind: ActionKind::WaitForMergeability,
        effect: ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(30),
            log: "Required checks are still pending on HEAD — wait and re-observe".into(),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingWait),
        blocker: BlockerKey::from_static("merge_blocked_check_pending"),
    }
}

fn merge_blocked_by_policy(state: &PullRequestProjection) -> Action {
    use ooda_core::{HandoffPrompt, NonEmpty, SingleLineString};
    let mut prompt = HandoffPrompt::new(
        "GitHub reports BLOCKED but no modeled gate (threads, review, \
         required checks) explains the blockage — likely an unmodeled \
         merge requirement such as signed commits, code-owner review, \
         deployment protection, or a custom branch ruleset. Inspect the \
         PR's Merge box on GitHub for the specific gate.",
    );
    // Signing requirements have their own structured axis
    // (`signing_eligibility`) that emits a Pathology HandoffHuman
    // directly — no soft prose diagnostic needed here.
    if let Some(rule_types) = NonEmpty::try_from_vec(
        state
            .active_branch_rule_types
            .iter()
            .map(|s| SingleLineString::new(s.clone()))
            .collect(),
    ) {
        prompt.push_paragraph("Active ruleset rules on this branch:".to_string());
        prompt.push_numbered_list(rule_types);
    }
    if !state.required_check_names_per_ruleset.is_empty() {
        prompt.push_paragraph(format!(
            "Required check names from ruleset: {}",
            state.required_check_names_per_ruleset.join(", "),
        ));
    }
    if !state.missing_required_check_names_on_head.is_empty() {
        prompt.push_paragraph(format!(
            "Missing on HEAD: {}",
            state.missing_required_check_names_on_head.join(", "),
        ));
    }
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::Pathology),
        blocker: BlockerKey::from_static("merge_blocked_policy"),
    }
}

fn merge_stale_threads(unresolved_total: usize) -> Action {
    use ooda_core::HandoffPrompt;
    let prompt = HandoffPrompt::new(format!(
        "Merge eligibility cross-check disagrees with mergeStateStatus. \
         Host reports the merge button is enabled, but {unresolved_total} \
         unresolved review thread(s) remain on a branch whose policy \
         requires conversation resolution. This is a stale-state signal \
         — GitHub will refuse the merge until the threads are resolved. \
         Open the PR on GitHub and resolve them."
    ));
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        // BlockingHuman (not Pathology): the reviews axis owns the
        // agent-fix path for unresolved threads at BlockingFix. This
        // cross-check diagnostic informs but must not shadow the
        // active agent work.
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("merge_stale_threads"),
    }
}

fn merge_stale_checks(rollup: RequiredChecksRollup) -> Action {
    use ooda_core::HandoffPrompt;
    let detail = match rollup {
        RequiredChecksRollup::Failure => "a required check is failing on HEAD",
        RequiredChecksRollup::Pending => "a required check is still pending on HEAD",
        // Caller filters Clean out before constructing this candidate.
        RequiredChecksRollup::Clean => "required-checks rollup disagrees with mergeStateStatus",
    };
    let prompt = HandoffPrompt::new(format!(
        "Merge eligibility cross-check disagrees with mergeStateStatus. \
         Host reports the merge button is enabled, but the independent \
         required-checks rollup says {detail}. This is a stale-state \
         signal — re-observe or inspect the CI axis for the specific \
         contexts."
    ));
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        // BlockingHuman (not Pathology): the ci axis owns the active
        // path (FixCi / EscalateCiFailed at BlockingFix or
        // BlockingHuman). This cross-check diagnostic must not
        // shadow that active work.
        urgency: Urgency::Mid(MidTier::BlockingHuman),
        blocker: BlockerKey::from_static("merge_stale_checks"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Timestamp;
    use crate::observe::github::pull_request_view::Mergeable;
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, FailedCheck};
    use crate::orient::thread::{
        BotName, FilePath, ReviewThread, ThreadAuthor, ThreadId, ThreadLocation,
    };

    fn state_with(status: MergeStateStatus) -> PullRequestProjection {
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
            merge_state_status: status,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
            active_branch_rule_types: vec![],
            required_check_names_per_ruleset: vec![],
            missing_required_check_names_on_head: vec![],
            conversation_resolution_required: false,
            signatures_required: false,
            unsigned_commits: vec![],
            required_approving_review_count: None,
        }
    }

    fn reviews_with(decision: Option<ReviewDecision>) -> ReviewSummary {
        use crate::orient::reviews::{PendingReviews, RequestedReviewerSet};
        ReviewSummary {
            decision,
            threads_unresolved: 0,
            threads_total: 0,
            bot_comments: 0,
            approvals_on_head: 0,
            approvals_stale: 0,
            pending_reviews: PendingReviews::default(),
            bot_reviews: vec![],
            requested_reviewers: RequestedReviewerSet::default(),
            latest_human_changes_requested: None,
        }
    }

    fn ci_clean() -> CiReport {
        CiReport {
            summary: CiSummary {
                required: CheckBucket::default(),
                missing_names: vec![],
                completed_at: None,
                advisory: CheckBucket::default(),
            },
            activity: CiActivity::Idle,
        }
    }

    fn ci_with_failed_required() -> CiReport {
        let mut ci = ci_clean();
        ci.summary.required.failed.push(FailedCheck {
            name: crate::ids::CheckName::parse("Build").unwrap(),
            description: String::new(),
            link: String::new(),
        });
        ci
    }

    fn ci_with_pending_required() -> CiReport {
        let mut ci = ci_clean();
        ci.summary
            .required
            .pending_names
            .push(crate::ids::CheckName::parse("Build").unwrap());
        ci
    }

    /// CI in `Resolved::MissingRequired` with a healthy in-flight
    /// producer on HEAD — the fan-in shape: a required aggregator
    /// check is missing, but a non-required workflow it depends on
    /// is still running normally.
    fn ci_missing_required_with_healthy_producer() -> CiReport {
        use crate::observe::github::workflow_runs::WorkflowRunId;
        let mut ci = ci_clean();
        ci.summary.missing_names =
            vec![crate::ids::CheckName::parse("Mergeability Check").unwrap()];
        ci.activity = CiActivity::Resolved(ResolvedState::MissingRequired {
            names: vec![crate::ids::CheckName::parse("Mergeability Check").unwrap()],
            stuck_runs: vec![],
            healthy_in_flight_runs: vec![WorkflowRunId(42)],
        });
        ci
    }

    /// CI in `Resolved::MissingRequired` with NO in-flight producers
    /// (every workflow has completed) — the unmodeled-gate shape:
    /// nothing is going to resolve the missing required check on
    /// its own.
    fn ci_missing_required_with_no_producer() -> CiReport {
        let mut ci = ci_clean();
        ci.summary.missing_names =
            vec![crate::ids::CheckName::parse("Mergeability Check").unwrap()];
        ci.activity = CiActivity::Resolved(ResolvedState::MissingRequired {
            names: vec![crate::ids::CheckName::parse("Mergeability Check").unwrap()],
            stuck_runs: vec![],
            healthy_in_flight_runs: vec![],
        });
        ci
    }

    fn live_thread(id: &str) -> ReviewThread {
        ReviewThread {
            id: ThreadId::new(id.to_string()).unwrap(),
            author: ThreadAuthor::Bot(BotName::Copilot),
            location: ThreadLocation {
                path: FilePath::new("src/foo.rs").unwrap(),
                line: Some(1),
            },
            body: "x".into(),
            state: ThreadState::Live,
            originating_comment_id: None,
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
        }
    }

    fn outdated_thread(id: &str) -> ReviewThread {
        let mut t = live_thread(id);
        t.state = ThreadState::Outdated;
        t.location.line = None;
        t
    }

    fn outdated_human_thread(id: &str) -> ReviewThread {
        let mut t = outdated_thread(id);
        t.author = ThreadAuthor::Human(crate::ids::GitHubLogin::parse("alice").unwrap());
        t
    }

    fn live_human_thread(id: &str) -> ReviewThread {
        let mut t = live_thread(id);
        t.author = ThreadAuthor::Human(crate::ids::GitHubLogin::parse("alice").unwrap());
        t
    }

    fn resolved_thread(id: &str) -> ReviewThread {
        let mut t = live_thread(id);
        t.state = ThreadState::Resolved;
        t
    }

    fn assert_blocker(candidates: &[Action], expected: &str) {
        assert_eq!(
            candidates.len(),
            1,
            "expected one candidate, got {:?}",
            candidates
                .iter()
                .map(|a| a.blocker.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(candidates[0].blocker.as_str(), expected);
    }

    // ── Invariant I5: lifecycle-terminal is owned upstream. This
    // ── module never sees terminal PRs because decide_from_candidates
    // ── short-circuits earlier; not tested here.

    // ── Invariant I6: UNKNOWN states emit Wait, not Blocks. ─────────

    #[test]
    fn unknown_merge_state_emits_wait() {
        let cs = merge_eligibility_candidates(
            &state_with(MergeStateStatus::Unknown),
            &[],
            &reviews_with(None),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_state_unknown");
        assert!(cs[0].effect.is_wait());
    }

    #[test]
    fn has_hooks_emits_wait() {
        let cs = merge_eligibility_candidates(
            &state_with(MergeStateStatus::HasHooks),
            &[],
            &reviews_with(None),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_state_has_hooks");
        assert!(cs[0].effect.is_wait());
    }

    // ── Behind / Draft / Dirty: other axes own these. ─────────────

    #[test]
    fn behind_is_silent() {
        assert!(
            merge_eligibility_candidates(
                &state_with(MergeStateStatus::Behind),
                &[],
                &reviews_with(None),
                &ci_clean(),
            )
            .is_empty()
        );
    }

    #[test]
    fn draft_is_silent() {
        assert!(
            merge_eligibility_candidates(
                &state_with(MergeStateStatus::Draft),
                &[],
                &reviews_with(None),
                &ci_clean(),
            )
            .is_empty()
        );
    }

    #[test]
    fn dirty_is_silent() {
        assert!(
            merge_eligibility_candidates(
                &state_with(MergeStateStatus::Dirty),
                &[],
                &reviews_with(None),
                &ci_clean(),
            )
            .is_empty()
        );
    }

    // ── F-E: outdated unresolved thread + BLOCKED + rule on. ──────

    #[test]
    fn f_e_blocked_with_outdated_bot_thread_only_stays_silent() {
        // Outdated-bot-only suppression: the reviews axis owns
        // these via `AddressThreads` at `BlockingFix`. This axis
        // staying silent prevents a tier inversion that would
        // otherwise route the agent-addressable case to a human.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![outdated_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert!(
            cs.is_empty(),
            "outdated-bot-only must not fire merge_blocked_threads; got {cs:?}"
        );
    }

    #[test]
    fn f_e_blocked_with_outdated_human_thread_fires_threads() {
        // Outdated human-authored thread: the agent must NOT
        // unilaterally resolve a human's stale concern. Fires the
        // human-handoff path at BlockingHuman.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![outdated_human_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    #[test]
    fn blocked_with_mixed_outdated_bot_and_human_fires_threads() {
        // Mixed: any non-(outdated-bot) unresolved entry breaks
        // the suppression — outdated humans need a human verdict.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![outdated_thread("T1"), outdated_human_thread("T2")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    #[test]
    fn merge_blocked_threads_fires_at_blocking_human_not_pathology() {
        // Tier invariant: this gate names a specific cause with an
        // actionable agent path elsewhere (reviews axis at
        // BlockingFix). It must NOT outrank `AddressThreads` via
        // Pathology — that was the latent bug that put live-thread
        // PRs in a permanent HandoffHuman loop.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_human_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
    }

    #[test]
    fn merge_blocked_review_fires_at_blocking_human_not_pathology() {
        // Same demotion as merge_blocked_threads: this names a
        // specific cause (ReviewRequired / ChangesRequested) the
        // reviews axis handles at BlockingFix. Must not outrank
        // via Pathology.
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::ReviewRequired)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_review");
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
    }

    #[test]
    fn merge_blocked_check_failure_fires_at_blocking_human_not_pathology() {
        // Same demotion: ci axis handles required-check failures
        // at BlockingFix (via FixCi) or BlockingHuman (via
        // EscalateCiFailed). This gate must not outrank those.
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_blocked_check_failure");
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
    }

    #[test]
    fn pending_approval_fires_when_ruleset_requires_more_than_observed() {
        // The named-gate replacement for the policy fallback: when
        // the branch ruleset requires N approvals AND the PR has
        // fewer than N APPROVED reviews on HEAD AND reviewDecision
        // is empty (not ReviewRequired / ChangesRequested).
        let mut s = state_with(MergeStateStatus::Blocked);
        s.required_approving_review_count = Some(1);
        let mut r = reviews_with(None);
        r.approvals_on_head = 0;
        let cs = merge_eligibility_candidates(&s, &[], &r, &ci_clean());
        assert_blocker(&cs, "merge_blocked_pending_approval");
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("requires 1 approving review"));
        assert!(rendered.contains("Current count: 0"));
    }

    #[test]
    fn pending_approval_silent_when_approvals_meet_threshold() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.required_approving_review_count = Some(1);
        let mut r = reviews_with(None);
        r.approvals_on_head = 1;
        let cs = merge_eligibility_candidates(&s, &[], &r, &ci_clean());
        // Threshold met → falls through to policy fallback (Pathology)
        // because no other modeled cause and Clean rollup.
        assert_blocker(&cs, "merge_blocked_policy");
    }

    #[test]
    fn pending_approval_silent_when_required_count_is_zero() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.required_approving_review_count = Some(0);
        let r = reviews_with(None);
        let cs = merge_eligibility_candidates(&s, &[], &r, &ci_clean());
        assert_blocker(&cs, "merge_blocked_policy");
    }

    #[test]
    fn pending_approval_silent_when_no_ruleset_required_count() {
        // None → no ruleset rule present → no named gate. Falls
        // through to policy fallback.
        let s = state_with(MergeStateStatus::Blocked);
        let r = reviews_with(None);
        let cs = merge_eligibility_candidates(&s, &[], &r, &ci_clean());
        assert_blocker(&cs, "merge_blocked_policy");
    }

    #[test]
    fn pending_approval_does_not_fire_when_review_decision_says_review_required() {
        // Explicit ReviewRequired decision is more specific; that
        // branch fires first in drill_blocked.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.required_approving_review_count = Some(1);
        let mut r = reviews_with(Some(ReviewDecision::ReviewRequired));
        r.approvals_on_head = 0;
        let cs = merge_eligibility_candidates(&s, &[], &r, &ci_clean());
        assert_blocker(&cs, "merge_blocked_review");
    }

    #[test]
    fn merge_blocked_policy_stays_at_pathology() {
        // Pathology invariant preserved: the unmodeled-gate
        // fallback IS a closure-check pathology and must continue
        // to outrank Wait actions on the runner.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.signatures_required = true;
        s.active_branch_rule_types = vec!["required_signatures".into()];
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_policy");
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Pathology));
    }

    #[test]
    fn merge_stale_threads_fires_at_blocking_human_not_pathology() {
        // Demoted from Pathology: the reviews axis owns the
        // agent-fix path for unresolved threads at BlockingFix.
        // This cross-check diagnostic informs but must not shadow.
        let mut s = state_with(MergeStateStatus::Clean);
        s.conversation_resolution_required = true;
        let threads = vec![live_human_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_stale_threads");
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
    }

    #[test]
    fn merge_stale_checks_fires_at_blocking_human_not_pathology() {
        // Demoted from Pathology: the ci axis owns the active path
        // (FixCi / EscalateCiFailed) at BlockingFix or
        // BlockingHuman. This cross-check must not shadow.
        let s = state_with(MergeStateStatus::Clean);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_stale_checks");
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingHuman));
    }

    #[test]
    fn outdated_bot_only_suppression_works_for_botname_other() {
        // The suppression matches on `ThreadAuthor::Bot(_)` — every
        // BotName variant qualifies, not just Copilot / Cursor.
        // Verifies a graphite-app bot is also suppressed.
        use crate::ids::GitHubLogin;
        use crate::orient::thread::BotName;
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let mut t = outdated_thread("T1");
        t.author = ThreadAuthor::Bot(BotName::Other(GitHubLogin::parse("graphite-app").unwrap()));
        let cs = merge_eligibility_candidates(
            &s,
            &[t],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert!(cs.is_empty(), "graphite-app outdated must be suppressed");
    }

    #[test]
    fn live_bot_plus_outdated_bot_fires_threads_not_suppressed() {
        // Suppression requires EVERY unresolved to be outdated-bot.
        // A live bot thread in the mix breaks the predicate; merge
        // fires its handoff (which AddressThreads at BlockingFix
        // will then outrank at the runner).
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1"), outdated_thread("T2")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    #[test]
    fn live_human_plus_outdated_bot_fires_threads_not_suppressed() {
        // Live-human in mix → not all outdated-bot → fires.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_human_thread("T1"), outdated_thread("T2")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    #[test]
    fn no_conversation_resolution_required_stays_silent_on_threads() {
        // When the branch policy does NOT require conversation
        // resolution, threads don't gate merge from this axis —
        // even if they're unresolved.
        let s = state_with(MergeStateStatus::Blocked);
        // conversation_resolution_required defaults to false in state_with
        let threads = vec![live_thread("T1"), outdated_human_thread("T2")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        // Falls through to policy fallback since rollup is Clean and no
        // other cause is named.
        assert_blocker(&cs, "merge_blocked_policy");
    }

    #[test]
    fn empty_unresolved_set_stays_silent_on_threads_branch() {
        // No unresolved threads → no threads-branch emission.
        // Falls through to whatever else the drill picks; with
        // clean state it lands on policy.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![resolved_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_policy");
    }

    // ── Live unresolved + BLOCKED also fires threads (drill order). ──

    #[test]
    fn blocked_with_live_unresolved_thread_emits_threads() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1"), resolved_thread("T2")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    // ── Conversation-resolution rule off → threads branch skipped. ──

    #[test]
    fn blocked_with_threads_but_rule_off_skips_threads_branch() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = false;
        let threads = vec![live_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        // Falls through to policy because review approved + ci clean.
        assert_blocker(&cs, "merge_blocked_policy");
    }

    // ── BLOCKED + review required → review branch. ─────────────────

    #[test]
    fn blocked_with_review_required_emits_review() {
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::ReviewRequired)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_review");
    }

    #[test]
    fn blocked_with_changes_requested_emits_review() {
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::ChangesRequested)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_review");
    }

    // ── BLOCKED + required-check failure → check_failure. ──────────

    #[test]
    fn blocked_with_failed_required_check_emits_check_failure() {
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_blocked_check_failure");
    }

    // ── BLOCKED + required-check pending → check_pending. ──────────

    #[test]
    fn blocked_with_pending_required_check_emits_check_pending_wait() {
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_with_pending_required(),
        );
        assert_blocker(&cs, "merge_blocked_check_pending");
        assert!(cs[0].effect.is_wait());
    }

    // ── F-C: BLOCKED with no modeled cause → policy. ──────────────

    #[test]
    fn f_c_blocked_unmodeled_emits_policy_with_diagnostic() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.signatures_required = true;
        s.active_branch_rule_types = vec!["required_signatures".into()];
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_policy");
        let rendered = cs[0].rendered_payload();
        // The rule-types enumeration still surfaces "required_signatures"
        // as an active rule on the branch. The dedicated signing-axis
        // (decide::signing_eligibility) owns the substantive signing
        // surface — this axis only confirms the cross-check absence.
        assert!(rendered.contains("required_signatures"));
    }

    // ── F-F: BLOCKED + MissingRequired + healthy in-flight → silent. ──

    #[test]
    fn f_f_blocked_missing_required_with_healthy_producer_stays_silent() {
        // Fan-in shape: required aggregator check (e.g.,
        // `Mergeability Check`) is missing because its `needs:`
        // dependency (a non-required workflow) is still running
        // normally. Treating this as `merge_blocked_by_policy` at
        // Pathology over-escalates — the right outcome is to wait
        // for the in-flight workflow to complete. ci.rs emits
        // `WaitForCi(ci_missing)` at BlockingWait; this axis must
        // stay silent so that wait takes the iteration.
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_missing_required_with_healthy_producer(),
        );
        assert!(
            cs.is_empty(),
            "fan-in case must not emit; got {} candidate(s)",
            cs.len()
        );
    }

    #[test]
    fn f_f_blocked_missing_required_no_producer_still_emits_policy() {
        // Control: same BLOCKED + MissingRequired shape but NO
        // in-flight producer. Nothing is going to resolve the
        // missing check on its own; this IS the unmodeled-policy
        // gate. Axis must still emit `merge_blocked_policy` at
        // Pathology.
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_missing_required_with_no_producer(),
        );
        assert_blocker(&cs, "merge_blocked_policy");
    }

    #[test]
    fn f_f_thread_cause_still_wins_over_fan_in_suppression() {
        // Priority preservation: even when MissingRequired carries
        // a healthy producer (which would suppress policy), an
        // unresolved-threads cause is more specific and must still
        // fire. Suppression applies only to the policy fallback —
        // not to threads / review / check_failure.
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_missing_required_with_healthy_producer(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    // ── F-B: UNSTABLE + unresolved threads + rule on → stale_threads. ──

    #[test]
    fn f_b_unstable_with_unresolved_threads_emits_stale_threads() {
        let mut s = state_with(MergeStateStatus::Unstable);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_stale_threads");
    }

    // ── CLEAN + cross-check fires for stale required-check rollup. ──

    #[test]
    fn clean_with_failed_required_check_emits_stale_checks() {
        let s = state_with(MergeStateStatus::Clean);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_stale_checks");
    }

    // ── F-A: stale-resolved threads + BLOCKED + no modeled cause → policy.

    #[test]
    fn f_a_stale_resolved_blocked_emits_policy() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        // GraphQL says all resolved but host still gates — we can't
        // see the gate so policy fires with the rule list.
        s.active_branch_rule_types = vec!["pull_request".into()];
        let threads = vec![resolved_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::Approved)),
            &ci_clean(),
        );
        assert_blocker(&cs, "merge_blocked_policy");
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("pull_request"));
    }

    // ── Invariant I1: eligible-and-clean → axis silent. ────────────

    #[test]
    fn eligible_and_clean_axis_silent() {
        let s = state_with(MergeStateStatus::Clean);
        assert!(
            merge_eligibility_candidates(
                &s,
                &[],
                &reviews_with(Some(ReviewDecision::Approved)),
                &ci_clean(),
            )
            .is_empty()
        );
    }

    #[test]
    fn unstable_with_no_cross_check_issue_is_silent() {
        let s = state_with(MergeStateStatus::Unstable);
        assert!(
            merge_eligibility_candidates(
                &s,
                &[],
                &reviews_with(Some(ReviewDecision::Approved)),
                &ci_clean(),
            )
            .is_empty()
        );
    }

    // ── Drill priority: threads > review > check > policy. ────────

    #[test]
    fn drill_priority_threads_beats_review() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1")];
        let cs = merge_eligibility_candidates(
            &s,
            &threads,
            &reviews_with(Some(ReviewDecision::ReviewRequired)),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_blocked_threads");
    }

    #[test]
    fn drill_priority_review_beats_check() {
        let s = state_with(MergeStateStatus::Blocked);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            &reviews_with(Some(ReviewDecision::ReviewRequired)),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_blocked_review");
    }
}
