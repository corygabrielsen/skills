//! Typed view of `GET /repos/{o}/{r}/issues/{n}/comments` (REST).
//!
//! Issue-level comments on a PR — distinct from inline review
//! comments (which live on review threads). We model only the fields
//! downstream stages use; the REST response includes body, URLs,
//! timestamps, and reactions we do not need yet.

use serde::{Deserialize, Serialize};

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug};

use super::gh::{GhError, gh_json_paginate};

/// Per-page projection — `gh api --paginate --jq` runs this filter
/// against each page, emitting one JSON array per page.
/// `gh_json_paginate` concatenates the per-page arrays.
///
/// The `user.login // ""` fallback covers GitHub returning
/// `user: null` for deleted accounts, which would otherwise make
/// `GitHubLogin` deserialization (non-empty) fail and abort the
/// whole observe phase.
const COMMENT_JQ: &str = r#"[.[] | {id, user: {login: (.user.login // "[deleted]")}}]"#;

/// Fetch issue-level comments on a PR (not inline review comments).
pub fn fetch_issue_comments(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<IssueComment>, GhError> {
    let path = format!("repos/{slug}/issues/{pr}/comments");
    gh_json_paginate(&["api", &path, "--paginate", "--jq", COMMENT_JQ])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IssueComment {
    pub id: u64,
    pub user: CommentUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommentUser {
    pub login: GitHubLogin,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../test/fixtures/github/issue_comments.json");

    #[test]
    fn deserializes_full_fixture() {
        let comments: Vec<IssueComment> = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(comments.len(), 4);
        assert_eq!(comments[0].user.login.as_str(), "corygabrielsen");
        assert!(comments[1].user.login.is_bot());
        assert_eq!(comments[1].user.login.as_str(), "claude[bot]");
    }

    #[test]
    fn extra_fields_ignored() {
        let json = r#"[{
            "id": 1,
            "node_id": "abc",
            "url": "https://example",
            "body": "hi",
            "user": {"login": "alice", "id": 123, "site_admin": false}
        }]"#;
        let comments: Vec<IssueComment> = serde_json::from_str(json).unwrap();
        assert_eq!(comments[0].id, 1);
        assert_eq!(comments[0].user.login.as_str(), "alice");
    }
}
