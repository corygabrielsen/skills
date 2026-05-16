//! PR-meta candidates.
//!
//! Emit `SyncPrMeta` when the orient axis is `Drift` or
//! `NeverAttested` AND the PR has at least one commit. Skip for
//! `Synced` (no work) and for empty PRs (no commits to attest
//! against). Information-tier — advisory; never preempts a
//! mechanical merge blocker.

use std::path::Path;

use crate::act::sync_pr_meta::build_sync_pr_meta_prompt;
use crate::ids::{BlockerKey, PullRequestNumber};
use crate::orient::OrientedState;
use crate::orient::pr_meta::PrMetadata;

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

#[must_use]
pub fn candidates(oriented: &OrientedState, pr: PullRequestNumber) -> Vec<Action> {
    if oriented.state.commits == 0 {
        return Vec::new();
    }
    let needs_sync = matches!(
        oriented.pr_metadata,
        PrMetadata::Drift { .. } | PrMetadata::NeverAttested,
    );
    if !needs_sync {
        return Vec::new();
    }

    let attest_path: Option<&Path> = oriented.attest_path.as_deref();
    let prompt = build_sync_pr_meta_prompt(pr, &oriented.pr_metadata, attest_path);
    let kind = ActionKind::SyncPrMeta {
        attest_path: attest_path.map(Path::to_path_buf).unwrap_or_default(),
    };
    let blocker = match oriented.pr_metadata {
        PrMetadata::Drift { .. } => BlockerKey::tag("pr_meta_drift"),
        PrMetadata::NeverAttested => BlockerKey::tag("pr_meta_never_attested"),
        // Synced is filtered above; the match is exhaustive for
        // clippy, the arm is unreachable in practice.
        PrMetadata::Synced => BlockerKey::tag("pr_meta_synced"),
    };
    vec![Action {
        kind,
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::Hygiene,
        blocker,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, PullRequestNumber, Timestamp};
    use crate::observe::github::pr_view::{MergeStateStatus, Mergeable};
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestState;

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn pr_state(commits: usize) -> PullRequestState {
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
            commits,
            behind: false,
            has_open_parent_pr: false,
            merge_state_status: MergeStateStatus::Clean,
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            last_commit_at: None,
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

    fn oriented(commits: usize, pr_metadata: PrMetadata) -> OrientedState {
        OrientedState {
            ci: ci_report(),
            state: pr_state(commits),
            reviews: reviews(),
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
            pr_metadata,
            attest_path: Some(std::path::PathBuf::from("/state/753/pr_meta_attest.json")),
            codex_review: None,
        }
    }

    fn drift() -> PrMetadata {
        PrMetadata::Drift {
            attested_sha: GitCommitSha::parse(&"a".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
            head_sha: GitCommitSha::parse(&"b".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
            commits_behind: 2,
        }
    }

    #[test]
    fn drift_with_commits_emits_sync_pr_meta() {
        let cs = candidates(&oriented(3, drift()), pr());
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::SyncPrMeta { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Hygiene);
        assert_eq!(cs[0].target_effect, TargetEffect::Neutral);
        assert_eq!(cs[0].blocker.as_str(), "pr_meta_drift");
    }

    #[test]
    fn never_attested_with_commits_emits_sync_pr_meta() {
        let cs = candidates(&oriented(1, PrMetadata::NeverAttested), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "pr_meta_never_attested");
    }

    #[test]
    fn never_attested_with_zero_commits_emits_nothing() {
        let cs = candidates(&oriented(0, PrMetadata::NeverAttested), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn drift_with_zero_commits_emits_nothing() {
        let cs = candidates(&oriented(0, drift()), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn synced_emits_nothing() {
        let cs = candidates(&oriented(3, PrMetadata::Synced), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn sync_pr_meta_carries_attest_path_in_payload() {
        let cs = candidates(&oriented(3, drift()), pr());
        let ActionKind::SyncPrMeta { attest_path } = &cs[0].kind else {
            panic!("expected SyncPrMeta");
        };
        assert_eq!(
            attest_path,
            std::path::Path::new("/state/753/pr_meta_attest.json")
        );
    }

    #[test]
    fn sync_pr_meta_stall_key_distinguishes_drift_from_never_attested() {
        let drift_action = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        let never_action = candidates(&oriented(2, PrMetadata::NeverAttested), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_ne!(drift_action.stall_key(), never_action.stall_key());
    }

    #[test]
    fn sync_pr_meta_stall_key_equal_to_itself() {
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
    fn sync_pr_meta_action_name_is_sync_pr_meta() {
        let a = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.kind.name(), "SyncPrMeta");
    }
}
