//! Claude-review candidate.
//!
//! Content-keyed attestation: fires only when the axis reports
//! review content past the last attestation. The quiet states
//! (no surface to grade, already addressed) emit nothing because
//! there is no agent action to take. Hygiene tier — advisory
//! rather than blocking.

use crate::act::address_claude_review::build_address_claude_review_prompt;
use crate::ids::{BlockerKey, PullRequestNumber};
use crate::orient::OrientedState;
use crate::orient::claude_review::ClaudeReview;

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

#[must_use]
pub(super) fn candidates(oriented: &OrientedState, pr: PullRequestNumber) -> Vec<Action> {
    let ClaudeReview::Fresh {
        body_at,
        latest_claude_body,
        latest_claude_url,
        inline_thread_count,
        ..
    } = &oriented.claude_review
    else {
        return Vec::new();
    };
    let Some(attest_path) = oriented.claude_review_attest_path.as_deref() else {
        return Vec::new();
    };

    let prompt = build_address_claude_review_prompt(
        pr,
        *body_at,
        latest_claude_body,
        latest_claude_url,
        *inline_thread_count,
        Some(attest_path),
    );
    let kind = ActionKind::AddressClaudeReview {
        attest_path: attest_path.to_path_buf(),
    };
    vec![Action {
        kind,
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::Mid(MidTier::Hygiene),
        blocker: BlockerKey::from_static("claude_review_fresh"),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, PullRequestNumber, Timestamp};
    use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};
    use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
    use crate::orient::doc_review::DocReview;
    use crate::orient::pull_request_metadata::PullRequestMetadata;
    use crate::orient::reviews::{PendingReviews, ReviewSummary};
    use crate::orient::state::PullRequestProjection;
    use chrono::{DateTime, Utc};
    use ooda_core::MidTier;

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

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

    fn oriented(commits: usize, claude_review: ClaudeReview) -> OrientedState {
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
            doc_review: DocReview::Synced,
            doc_review_attest_path: None,
            claude_review,
            claude_review_attest_path: Some(std::path::PathBuf::from(
                "/state/753/claude_review_attest.json",
            )),
            closeout: crate::orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
        }
    }

    fn fresh() -> ClaudeReview {
        let at = DateTime::parse_from_rfc3339("2026-05-02T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        ClaudeReview::Fresh {
            latest_claude_at: at,
            body_at: at,
            latest_claude_body: "🔴 important".into(),
            latest_claude_url: "https://example/r/1".into(),
            inline_thread_count: 1,
            attested_at: None,
            head_sha: GitCommitSha::parse(HEAD_SHA).unwrap().as_str().to_string(),
        }
    }

    #[test]
    fn fresh_emits_address_claude_review() {
        let cs = candidates(&oriented(3, fresh()), pr());
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::AddressClaudeReview { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Hygiene));
        assert_eq!(cs[0].target_effect, TargetEffect::Neutral);
        assert_eq!(cs[0].blocker.as_str(), "claude_review_fresh");
    }

    #[test]
    fn no_activity_emits_nothing() {
        let cs = candidates(&oriented(3, ClaudeReview::NoActivity), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn addressed_emits_nothing() {
        let cs = candidates(&oriented(3, ClaudeReview::Addressed), pr());
        assert!(cs.is_empty());
    }

    #[test]
    fn fresh_with_no_attest_path_emits_nothing() {
        let mut o = oriented(3, fresh());
        o.claude_review_attest_path = None;
        assert!(candidates(&o, pr()).is_empty());
    }

    #[test]
    fn address_claude_review_carries_attest_path_in_payload() {
        let cs = candidates(&oriented(3, fresh()), pr());
        let ActionKind::AddressClaudeReview { attest_path } = &cs[0].kind else {
            panic!("expected AddressClaudeReview");
        };
        assert_eq!(
            attest_path,
            std::path::Path::new("/state/753/claude_review_attest.json")
        );
    }

    #[test]
    fn address_claude_review_action_name_is_address_claude_review() {
        let a = candidates(&oriented(3, fresh()), pr())
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(a.kind.name(), "AddressClaudeReview");
    }
}
