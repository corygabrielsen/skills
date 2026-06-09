//! Currently-pending reviewer requests, partitioned into user and
//! team rows. The model carries only the fields downstream stages
//! use.

use serde::{Deserialize, Serialize};

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug};

use super::gh::{GhError, gh_json};

/// Fetch currently-pending reviewer requests for a PR.
pub(crate) fn fetch_requested_reviewers(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<RequestedReviewers, GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/requested_reviewers");
    gh_json(&["api", &path])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub(crate) struct RequestedReviewers {
    #[serde(default)]
    pub users: Vec<RequestedUser>,
    #[serde(default)]
    pub teams: Vec<RequestedTeam>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RequestedUser {
    pub login: GitHubLogin,
    /// Identity class.
    #[serde(rename = "type")]
    pub user_type: UserType,
}

/// Identity class for a requested reviewer. `Unknown` is the
/// forward-compat fallback for host-introduced variants (e.g.
/// `Service` / `Orbot` / future identity types) — decode never
/// aborts the observe pass on a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) enum UserType {
    User,
    Bot,
    Organization,
    Mannequin,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RequestedTeam {
    pub slug: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../test/fixtures/github/requested_reviewers.json");

    #[test]
    fn deserializes_empty_fixture() {
        let rr: RequestedReviewers = serde_json::from_str(FIXTURE).unwrap();
        assert!(rr.users.is_empty());
        assert!(rr.teams.is_empty());
    }

    #[test]
    fn deserializes_mixed_shape() {
        let json = r#"{
            "users": [
                {"login": "alice", "type": "User", "id": 1},
                {"login": "copilot[bot]", "type": "Bot", "id": 2}
            ],
            "teams": [
                {"slug": "backend", "name": "Backend", "id": 10}
            ]
        }"#;
        let rr: RequestedReviewers = serde_json::from_str(json).unwrap();
        assert_eq!(rr.users.len(), 2);
        assert_eq!(rr.users[0].user_type, UserType::User);
        assert_eq!(rr.users[1].user_type, UserType::Bot);
        assert_eq!(rr.teams[0].slug, "backend");
    }

    #[test]
    fn unknown_user_type_decodes_as_unknown_variant() {
        // Pre-fix this rejected the entire fetch; post-fix the
        // unmodeled identity class lands on `Unknown` so the
        // observe pass survives. Future variants like `Service` /
        // `Orbot` decode cleanly.
        let json = r#"{"users":[{"login":"x","type":"Martian"}],"teams":[]}"#;
        let r: RequestedReviewers = serde_json::from_str(json).unwrap();
        assert_eq!(r.users.len(), 1);
        assert_eq!(r.users[0].user_type, UserType::Unknown);
    }
}
