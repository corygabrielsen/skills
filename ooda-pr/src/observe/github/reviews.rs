//! Typed projection of the host's review-submission stream.
//!
//! Each row is one submission event in the lifecycle (approval,
//! change request, comment, dismissal, pending). The model carries
//! only the fields downstream stages consume; unmodeled fields are
//! ignored by the decoder.

use serde::{Deserialize, Serialize};

use crate::ids::{GitCommitSha, GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json_paginate};

/// Fetch every review on a PR.
pub(crate) fn fetch_pull_request_reviews(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<PullRequestReview>, GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/reviews?per_page=100");
    gh_json_paginate(&["api", &path, "--paginate"])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct PullRequestReview {
    /// Absent when the author identity has been deleted. Optional so
    /// historical reviews do not abort the observe pass.
    #[serde(default)]
    pub user: Option<ReviewUser>,
    pub state: ReviewState,
    pub commit_id: GitCommitSha,
    /// Absent for un-submitted (pending) reviews.
    #[serde(default)]
    pub submitted_at: Option<Timestamp>,
    #[serde(default)]
    pub body: String,
    /// Permalink to the review. Optional so test fixtures without
    /// it decode; production responses always populate it.
    #[serde(default)]
    pub html_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ReviewUser {
    pub login: GitHubLogin,
}

/// Per-review verdict. `Unknown` is the forward-compat fallback
/// for host-introduced variants — decode never aborts the observe
/// pass on a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEWS_FIXTURE: &str = include_str!("../../../test/fixtures/github/pr_reviews.json");

    #[test]
    fn deserializes_full_fixture() {
        let reviews: Vec<PullRequestReview> = serde_json::from_str(REVIEWS_FIXTURE).unwrap();
        assert_eq!(reviews.len(), 2);

        // First review: Copilot commented.
        let user = reviews[0].user.as_ref().expect("fixture has user");
        assert_eq!(user.login.as_str(), "copilot-pull-request-reviewer[bot]");
        assert!(user.login.is_bot());
        assert_eq!(reviews[0].state, ReviewState::Commented);
        assert!(reviews[0].submitted_at.is_some());
        assert!(!reviews[0].body.is_empty());
    }

    #[test]
    fn pending_review_has_null_submitted_at() {
        let json = r#"[{
            "user": {"login": "alice"},
            "state": "PENDING",
            "commit_id": "0123456789abcdef0123456789abcdef01234567",
            "submitted_at": null,
            "body": ""
        }]"#;
        let reviews: Vec<PullRequestReview> = serde_json::from_str(json).unwrap();
        assert_eq!(reviews[0].state, ReviewState::Pending);
        assert_eq!(reviews[0].submitted_at, None);
        assert_eq!(reviews[0].body, "");
    }

    #[test]
    fn all_five_review_states_parse() {
        let cases = [
            ("APPROVED", ReviewState::Approved),
            ("CHANGES_REQUESTED", ReviewState::ChangesRequested),
            ("COMMENTED", ReviewState::Commented),
            ("DISMISSED", ReviewState::Dismissed),
            ("PENDING", ReviewState::Pending),
        ];
        for (s, expected) in cases {
            let json = format!(
                r#"[{{"user":{{"login":"a"}},"state":"{s}","commit_id":"{sha}","submitted_at":"2026-04-23T00:00:00Z","body":""}}]"#,
                sha = "a".repeat(40),
            );
            let reviews: Vec<PullRequestReview> = serde_json::from_str(&json).unwrap();
            assert_eq!(reviews[0].state, expected);
        }
    }

    #[test]
    fn unknown_review_state_decodes_as_unknown_variant() {
        // Pre-fix this aborted the entire reviews fetch and
        // crashed the observe pass. Post-fix the row decodes;
        // downstream axes treat Unknown as "verdict observed but
        // unmodeled" rather than as "no review".
        let json = format!(
            r#"[{{"user":{{"login":"a"}},"state":"WEIRD","commit_id":"{sha}","submitted_at":"2026-04-23T00:00:00Z","body":""}}]"#,
            sha = "a".repeat(40),
        );
        let reviews: Vec<PullRequestReview> = serde_json::from_str(&json).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].state, ReviewState::Unknown);
    }

    #[test]
    fn extra_fields_ignored() {
        // The real fixture includes id, node_id, html_url, _links,
        // avatar_url etc. — all ignored by our narrower struct.
        let reviews: Vec<PullRequestReview> = serde_json::from_str(REVIEWS_FIXTURE).unwrap();
        assert!(!reviews.is_empty());
    }
}
