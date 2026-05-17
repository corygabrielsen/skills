//! Closeout candidates.
//!
//! Emit `Closeout` when the axis is `Drift` or `NeverAttested`, at
//! `Urgency::Closeout` — strictly the least-urgent tier so the
//! reducer outranks Closeout with any other axis's candidate. The
//! gate fires only when every other axis is silent, making
//! `HandoffHuman` conditional on an agent-signed attestation at
//! current HEAD.
//!
//! No commit-count guard: Closeout fires on zero-commit PRs too.
//! The closeout is about pre-handoff sign-off, not hygiene-when-
//! there's-work.

use std::path::Path;

use crate::act::closeout::build_closeout_prompt;
use crate::ids::{BlockerKey, PullRequestNumber};
use crate::orient::OrientedState;
use crate::orient::closeout::Closeout;

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

#[must_use]
pub(super) fn candidates(oriented: &OrientedState, pr: PullRequestNumber) -> Vec<Action> {
    let needs_closeout = matches!(
        oriented.closeout,
        Closeout::Drift { .. } | Closeout::NeverAttested,
    );
    if !needs_closeout {
        return Vec::new();
    }
    let Some(attest_path) = oriented.closeout_attest_path.as_deref() else {
        return Vec::new();
    };

    let attest_path_opt: Option<&Path> = Some(attest_path);
    let prompt = build_closeout_prompt(pr, &oriented.closeout, attest_path_opt);
    let kind = ActionKind::Closeout {
        attest_path: attest_path.to_path_buf(),
    };
    let blocker = match oriented.closeout {
        Closeout::Drift { .. } => BlockerKey::from_static("closeout_drift"),
        Closeout::NeverAttested => BlockerKey::from_static("closeout_never_attested"),
        Closeout::Synced => BlockerKey::from_static("closeout_synced"),
    };
    vec![Action {
        kind,
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::Closeout,
        blocker,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, PullRequestNumber, Timestamp};
    use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::pull_request_metadata::PullRequestMetadata;
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestProjection;

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

    fn oriented(commits: usize, closeout: Closeout) -> OrientedState {
        OrientedState {
            ci: ci_report(),
            state: pull_request_state(commits),
            reviews: reviews(),
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
            pull_request_metadata: PullRequestMetadata::Synced,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::Synced,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            codex_review: None,
            closeout,
            closeout_attest_path: Some(std::path::PathBuf::from("/state/753/closeout_attest.json")),
        }
    }

    fn drift() -> Closeout {
        Closeout::Drift {
            attested_sha: GitCommitSha::parse(&"a".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
            head_sha: GitCommitSha::parse(&"b".repeat(40))
                .unwrap()
                .as_str()
                .to_string(),
        }
    }

    #[test]
    fn drift_emits_closeout_at_closeout_urgency() {
        let cs = candidates(&oriented(3, drift()), pr());
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::Closeout { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Closeout);
        assert_eq!(cs[0].target_effect, TargetEffect::Neutral);
        assert_eq!(cs[0].blocker.as_str(), "closeout_drift");
    }

    #[test]
    fn never_attested_emits_closeout() {
        let cs = candidates(&oriented(1, Closeout::NeverAttested), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "closeout_never_attested");
    }

    #[test]
    fn never_attested_with_zero_commits_still_emits() {
        // Distinct from PR-metadata / doc-review: Closeout fires even
        // on a zero-commit PR. The closeout is about pre-handoff sign-
        // off, not hygiene-when-there's-work.
        let cs = candidates(&oriented(0, Closeout::NeverAttested), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "closeout_never_attested");
    }

    #[test]
    fn drift_with_zero_commits_still_emits() {
        let cs = candidates(&oriented(0, drift()), pr());
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].blocker.as_str(), "closeout_drift");
    }

    #[test]
    fn synced_emits_nothing() {
        let cs = candidates(&oriented(3, Closeout::Synced), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn closeout_carries_attest_path_in_payload() {
        let cs = candidates(&oriented(3, drift()), pr());
        let ActionKind::Closeout { attest_path } = &cs[0].kind else {
            panic!("expected Closeout");
        };
        assert_eq!(
            attest_path,
            std::path::Path::new("/state/753/closeout_attest.json")
        );
    }

    #[test]
    fn closeout_stall_key_distinguishes_drift_from_never_attested() {
        let drift_action = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        let never_action = candidates(&oriented(2, Closeout::NeverAttested), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_ne!(drift_action.stall_key(), never_action.stall_key());
    }

    #[test]
    fn closeout_stall_key_equal_to_itself() {
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
        o.closeout_attest_path = None;
        assert!(candidates(&o, pr()).is_empty());
    }

    #[test]
    fn never_attested_with_no_attest_path_emits_nothing() {
        let mut o = oriented(3, Closeout::NeverAttested);
        o.closeout_attest_path = None;
        assert!(candidates(&o, pr()).is_empty());
    }

    #[test]
    fn closeout_action_name_is_closeout() {
        let a = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.kind.name(), "Closeout");
    }

    #[test]
    fn closeout_is_strictly_least_urgent_in_decide() {
        let a = candidates(&oriented(2, drift()), pr())
            .into_iter()
            .next()
            .unwrap();
        // The reducer's `max` over urgency will only select Closeout
        // when no other axis emitted anything; this assertion documents
        // the structural invariant.
        assert!(Urgency::Hygiene < a.urgency);
        assert!(Urgency::Critical < a.urgency);
    }
}
