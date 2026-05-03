//! Typed view of `GET /repos/{o}/{r}/pulls/{n}/reviews` (REST).
//!
//! Each review is a single submission event — approval, change request,
//! comment, dismissal, or pending. We model only the fields later
//! stages consume; REST returns far more (avatar URLs, `_links`, etc.)
//! which serde silently ignores.

use serde::Deserialize;

use crate::ids::{GitCommitSha, GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{gh_json_paginate, GhError};

/// Fetch all reviews on a PR. `gh api --paginate` emits one JSON
/// array per page; `gh_json_paginate` concatenates them.
pub fn fetch_pr_reviews(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<PullRequestReview>, GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/reviews");
    gh_json_paginate(&["api", &path, "--paginate"])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestReview {
    /// Null when the review's author has been deleted. Optional
    /// so observe doesn't abort on historical reviews — same
    /// pattern as `ThreadComment.author` and the comment-fetcher
    /// jq fallback.
    #[serde(default)]
    pub user: Option<ReviewUser>,
    pub state: ReviewState,
    pub commit_id: GitCommitSha,
    /// Null for `PENDING` reviews that have not yet been submitted.
    #[serde(default)]
    pub submitted_at: Option<Timestamp>,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewUser {
    pub login: GitHubLogin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEWS_FIXTURE: &str =
        include_str!("../../../test/fixtures/github/pr_reviews.json");

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
    fn rejects_unknown_review_state() {
        let json = format!(
            r#"[{{"user":{{"login":"a"}},"state":"WEIRD","commit_id":"{sha}","submitted_at":"2026-04-23T00:00:00Z","body":""}}]"#,
            sha = "a".repeat(40),
        );
        let err = serde_json::from_str::<Vec<PullRequestReview>>(&json).unwrap_err();
        assert!(err.to_string().contains("unknown variant") || err.to_string().contains("WEIRD"));
    }

    #[test]
    fn extra_fields_ignored() {
        // The real fixture includes id, node_id, html_url, _links,
        // avatar_url etc. — all ignored by our narrower struct.
        let reviews: Vec<PullRequestReview> = serde_json::from_str(REVIEWS_FIXTURE).unwrap();
        assert!(!reviews.is_empty());
    }
}
