//! Issue-level comments on a PR, distinct from inline review
//! comments anchored on review threads. The model carries only the
//! fields downstream stages use.

use serde::{Deserialize, Serialize};

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json_paginate};

/// Server-side per-page projection. The deleted-identity fallback
/// is required so deleted-author rows do not abort decoding (the
/// login type rejects empty strings).
const COMMENT_JQ: &str =
    r#"[.[] | {id, user: {login: (.user.login // "[deleted]")}, body, created_at, html_url}]"#;

/// Fetch every issue-level comment on a PR.
pub(crate) fn fetch_issue_comments(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<IssueComment>, GhError> {
    let path = format!("repos/{slug}/issues/{pr}/comments?per_page=100");
    gh_json_paginate(&["api", &path, "--paginate", "--jq", COMMENT_JQ])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct IssueComment {
    pub id: u64,
    pub user: CommentUser,
    /// Body content. Default-empty so legacy fixtures decode.
    #[serde(default)]
    pub body: String,
    /// Creation timestamp. Default-epoch so legacy fixtures decode.
    #[serde(default = "default_timestamp")]
    pub created_at: Timestamp,
    /// Permalink. Default-empty so legacy fixtures decode.
    #[serde(default)]
    pub html_url: String,
}

fn default_timestamp() -> Timestamp {
    Timestamp::parse("1970-01-01T00:00:00Z").expect("epoch parses")
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct CommentUser {
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
            "created_at": "2026-04-23T10:00:00Z",
            "html_url": "https://github.com/o/r/pull/1#issuecomment-1",
            "user": {"login": "alice", "id": 123, "site_admin": false}
        }]"#;
        let comments: Vec<IssueComment> = serde_json::from_str(json).unwrap();
        assert_eq!(comments[0].id, 1);
        assert_eq!(comments[0].user.login.as_str(), "alice");
        assert_eq!(comments[0].body, "hi");
    }

    #[test]
    fn legacy_fixture_without_body_fields_still_deserializes() {
        let json = r#"[{"id":1,"user":{"login":"alice"}}]"#;
        let comments: Vec<IssueComment> = serde_json::from_str(json).unwrap();
        assert_eq!(comments[0].body, "");
        assert_eq!(comments[0].html_url, "");
    }
}
