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
//! - **Outdated-unresolved threads count**: GitHub's conversation-
//!   resolution gate counts every non-resolved thread, including
//!   outdated ones. This module counts them too — the reviews
//!   axis's `!is_outdated` filter applies to "agent-addressable",
//!   not to "merge-gating".
//! - **Stale-state cross-checks on green status**: when
//!   `mergeStateStatus ∈ {CLEAN, UNSTABLE, HAS_HOOKS}` the host
//!   says go, but the cross-checks for conversation-resolution and
//!   check-rollup are re-verified independently because BLOCKED ↔
//!   CLEAN transitions lag in known real bugs.

use crate::ids::BlockerKey;
use crate::observe::github::pull_request_view::{MergeStateStatus, ReviewDecision};
use crate::orient::ci::CiReport;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::{ReviewThread, ThreadState};

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
    review_decision: Option<ReviewDecision>,
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
        MergeStateStatus::Blocked => {
            vec![drill_blocked(
                state,
                unresolved_total,
                review_decision,
                rollup,
            )]
        }
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
fn drill_blocked(
    state: &PullRequestProjection,
    unresolved_total: usize,
    review_decision: Option<ReviewDecision>,
    rollup: RequiredChecksRollup,
) -> Action {
    if state.conversation_resolution_required && unresolved_total > 0 {
        return merge_blocked_by_threads(unresolved_total);
    }
    if matches!(
        review_decision,
        Some(ReviewDecision::ReviewRequired | ReviewDecision::ChangesRequested)
    ) {
        return merge_blocked_by_review(review_decision);
    }
    match rollup {
        RequiredChecksRollup::Failure => merge_blocked_by_check_failure(),
        RequiredChecksRollup::Pending => merge_blocked_by_check_pending(),
        RequiredChecksRollup::Clean => merge_blocked_by_policy(state),
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
         outdated threads — outdated threads still gate merge even \
         though they are not agent-addressable). Open the PR on GitHub \
         and resolve the remaining conversation(s)."
    ));
    Action {
        kind: ActionKind::ResolveMergePolicy,
        effect: ActionEffect::Human { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::Pathology),
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
        urgency: Urgency::Mid(MidTier::Pathology),
        blocker: BlockerKey::from_static("merge_blocked_review"),
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
        urgency: Urgency::Mid(MidTier::Pathology),
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
    if state.signatures_required {
        prompt.push_paragraph(
            "Signed commits are required on this branch — confirm every \
             commit on HEAD is signed."
                .to_string(),
        );
    }
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
        urgency: Urgency::Mid(MidTier::Pathology),
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
        urgency: Urgency::Mid(MidTier::Pathology),
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
            created_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
        }
    }

    fn outdated_thread(id: &str) -> ReviewThread {
        let mut t = live_thread(id);
        t.state = ThreadState::Outdated;
        t.location.line = None;
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
            None,
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
            None,
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
                None,
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
                None,
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
                None,
                &ci_clean(),
            )
            .is_empty()
        );
    }

    // ── F-E: outdated unresolved thread + BLOCKED + rule on. ──────

    #[test]
    fn f_e_blocked_with_outdated_unresolved_thread_emits_threads() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![outdated_thread("T1")];
        let cs =
            merge_eligibility_candidates(&s, &threads, Some(ReviewDecision::Approved), &ci_clean());
        assert_blocker(&cs, "merge_blocked_threads");
    }

    // ── Live unresolved + BLOCKED also fires threads (drill order). ──

    #[test]
    fn blocked_with_live_unresolved_thread_emits_threads() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1"), resolved_thread("T2")];
        let cs =
            merge_eligibility_candidates(&s, &threads, Some(ReviewDecision::Approved), &ci_clean());
        assert_blocker(&cs, "merge_blocked_threads");
    }

    // ── Conversation-resolution rule off → threads branch skipped. ──

    #[test]
    fn blocked_with_threads_but_rule_off_skips_threads_branch() {
        let mut s = state_with(MergeStateStatus::Blocked);
        s.conversation_resolution_required = false;
        let threads = vec![live_thread("T1")];
        let cs =
            merge_eligibility_candidates(&s, &threads, Some(ReviewDecision::Approved), &ci_clean());
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
            Some(ReviewDecision::ReviewRequired),
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
            Some(ReviewDecision::ChangesRequested),
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
            Some(ReviewDecision::Approved),
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
            Some(ReviewDecision::Approved),
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
        let cs = merge_eligibility_candidates(&s, &[], Some(ReviewDecision::Approved), &ci_clean());
        assert_blocker(&cs, "merge_blocked_policy");
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Signed commits are required"));
        assert!(rendered.contains("required_signatures"));
    }

    // ── F-B: UNSTABLE + unresolved threads + rule on → stale_threads. ──

    #[test]
    fn f_b_unstable_with_unresolved_threads_emits_stale_threads() {
        let mut s = state_with(MergeStateStatus::Unstable);
        s.conversation_resolution_required = true;
        let threads = vec![live_thread("T1")];
        let cs =
            merge_eligibility_candidates(&s, &threads, Some(ReviewDecision::Approved), &ci_clean());
        assert_blocker(&cs, "merge_stale_threads");
    }

    // ── CLEAN + cross-check fires for stale required-check rollup. ──

    #[test]
    fn clean_with_failed_required_check_emits_stale_checks() {
        let s = state_with(MergeStateStatus::Clean);
        let cs = merge_eligibility_candidates(
            &s,
            &[],
            Some(ReviewDecision::Approved),
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
        let cs =
            merge_eligibility_candidates(&s, &threads, Some(ReviewDecision::Approved), &ci_clean());
        assert_blocker(&cs, "merge_blocked_policy");
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("pull_request"));
    }

    // ── Invariant I1: eligible-and-clean → axis silent. ────────────

    #[test]
    fn eligible_and_clean_axis_silent() {
        let s = state_with(MergeStateStatus::Clean);
        assert!(
            merge_eligibility_candidates(&s, &[], Some(ReviewDecision::Approved), &ci_clean(),)
                .is_empty()
        );
    }

    #[test]
    fn unstable_with_no_cross_check_issue_is_silent() {
        let s = state_with(MergeStateStatus::Unstable);
        assert!(
            merge_eligibility_candidates(&s, &[], Some(ReviewDecision::Approved), &ci_clean(),)
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
            Some(ReviewDecision::ReviewRequired),
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
            Some(ReviewDecision::ReviewRequired),
            &ci_with_failed_required(),
        );
        assert_blocker(&cs, "merge_blocked_review");
    }
}
