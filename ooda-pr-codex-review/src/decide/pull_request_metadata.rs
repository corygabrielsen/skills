//! PR-metadata sync candidate.
//!
//! SHA-keyed attestation: fires when the axis is unsynced and the
//! PR carries at least one commit (an empty PR has no work to
//! attest against). Hygiene tier — advisory rather than blocking.

use std::path::Path;

use crate::act::sync_pull_request_metadata::build_sync_pull_request_metadata_prompt;
use crate::ids::{BlockerKey, PullRequestNumber};
use crate::orient::OrientedState;
use crate::orient::pull_request_metadata::PullRequestMetadata;

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

#[must_use]
pub(crate) fn candidates(oriented: &OrientedState, pr: PullRequestNumber) -> Vec<Action> {
    if oriented.state.commits == 0 {
        return Vec::new();
    }
    let needs_sync = matches!(
        oriented.pull_request_metadata,
        PullRequestMetadata::Drift { .. } | PullRequestMetadata::NeverAttested,
    );
    if !needs_sync {
        return Vec::new();
    }
    let Some(attest_path) = oriented.attest_path.as_deref() else {
        return Vec::new();
    };

    let attest_path_opt: Option<&Path> = Some(attest_path);
    let prompt = build_sync_pull_request_metadata_prompt(
        pr,
        &oriented.pull_request_metadata,
        attest_path_opt,
    );
    let kind = ActionKind::SyncPullRequestMetadata {
        attest_path: attest_path.to_path_buf(),
    };
    // Distinct gate identity per state, so a transition between
    // unsynced states is not masked as a stall. The Synced arm is
    // unreachable in practice — filtered upstream — and exists
    // only to keep the match exhaustive.
    let blocker = match oriented.pull_request_metadata {
        PullRequestMetadata::Drift { .. } => BlockerKey::from_static("pr_meta_drift"),
        PullRequestMetadata::NeverAttested => BlockerKey::from_static("pr_meta_never_attested"),
        PullRequestMetadata::Synced => BlockerKey::from_static("pr_meta_synced"),
    };
    // State-conditional urgency: first-attestation is `Opening`
    // (fires before anything else); drift maintenance is `Hygiene`
    // (deferred behind blockers, never starves the loop).
    let urgency = match oriented.pull_request_metadata {
        PullRequestMetadata::NeverAttested => Urgency::Pre,
        PullRequestMetadata::Drift { .. } | PullRequestMetadata::Synced => {
            Urgency::Mid(MidTier::Hygiene)
        }
    };
    vec![Action {
        kind,
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Neutral,
        urgency,
        blocker,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, PullRequestNumber, Timestamp};
    use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestProjection;
    use ooda_core::MidTier;

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn pull_request_state(commits: usize) -> PullRequestProjection {
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
            commits,
            behind: false,
            has_open_parent_pr: false,
            merge_state_status: MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
            active_branch_rule_types: vec![],
            required_check_names_per_ruleset: vec![],
            missing_required_check_names_on_head: vec![],
        }
    }

    fn reviews() -> ReviewSummary {
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

    fn ci_report() -> CiReport {
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

    fn oriented(commits: usize, pull_request_metadata: PullRequestMetadata) -> OrientedState {
        OrientedState {
            ci: ci_report(),
            state: pull_request_state(commits),
            reviews: reviews(),
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
            pull_request_metadata,
            attest_path: Some(std::path::PathBuf::from("/state/753/pr_meta_attest.json")),
            doc_review: crate::orient::doc_review::DocReview::Synced,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            codex_review: None,
            closeout: crate::orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
        }
    }

    fn drift() -> PullRequestMetadata {
        PullRequestMetadata::Drift {
            attested_sha: GitCommitSha::parse(&"a".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
            head_sha: GitCommitSha::parse(&"b".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
            commits_behind: Some(2),
        }
    }

    #[test]
    fn drift_with_commits_emits_sync_pull_request_metadata() {
        let cs = candidates(&oriented(3, drift()), pr());
        assert_eq!(cs.len(), 1);
        assert!(matches!(
            cs[0].kind,
            ActionKind::SyncPullRequestMetadata { .. }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Hygiene));
        assert_eq!(cs[0].target_effect, TargetEffect::Neutral);
        assert_eq!(cs[0].blocker.as_str(), "pr_meta_drift");
    }

    #[test]
    fn never_attested_with_commits_emits_sync_pull_request_metadata() {
        let cs = candidates(&oriented(1, PullRequestMetadata::NeverAttested), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "pr_meta_never_attested");
        // State-conditional urgency: first-attestation fires at the
        // top tier so the agent's initial sign-off preempts every
        // other axis (CI wait, mechanical setup).
        assert_eq!(cs[0].urgency, Urgency::Pre);
    }

    #[test]
    fn drift_keeps_hygiene_urgency() {
        // Mid-cycle drift is deferred behind blockers; only
        // first-attestation gets the Opening boost.
        let cs = candidates(&oriented(3, drift()), pr());
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Hygiene));
    }

    #[test]
    fn never_attested_with_zero_commits_emits_nothing() {
        let cs = candidates(&oriented(0, PullRequestMetadata::NeverAttested), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn drift_with_zero_commits_emits_nothing() {
        let cs = candidates(&oriented(0, drift()), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn synced_emits_nothing() {
        let cs = candidates(&oriented(3, PullRequestMetadata::Synced), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn sync_pull_request_metadata_carries_attest_path_in_payload() {
        let cs = candidates(&oriented(3, drift()), pr());
        let ActionKind::SyncPullRequestMetadata { attest_path } = &cs[0].kind else {
            panic!("expected SyncPullRequestMetadata");
        };
        assert_eq!(
            attest_path,
            std::path::Path::new("/state/753/pr_meta_attest.json")
        );
    }

    #[test]
    fn sync_pull_request_metadata_stall_key_distinguishes_drift_from_never_attested() {
        let drift_action = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        let never_action = candidates(&oriented(2, PullRequestMetadata::NeverAttested), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_ne!(drift_action.stall_key(), never_action.stall_key());
    }

    #[test]
    fn sync_pull_request_metadata_stall_key_equal_to_itself() {
        let a = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        let b = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.stall_key(), b.stall_key());
    }

    #[test]
    fn drift_with_no_attest_path_emits_nothing() {
        let mut o = oriented(3, drift());
        o.attest_path = None;
        assert!(candidates(&o, pr()).is_empty());
    }

    #[test]
    fn never_attested_with_no_attest_path_emits_nothing() {
        let mut o = oriented(3, PullRequestMetadata::NeverAttested);
        o.attest_path = None;
        assert!(candidates(&o, pr()).is_empty());
    }

    #[test]
    fn sync_pull_request_metadata_action_name_is_sync_pull_request_metadata() {
        let a = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.kind.name(), "SyncPullRequestMetadata");
    }
}
