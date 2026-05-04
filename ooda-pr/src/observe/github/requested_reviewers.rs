//! Typed view of `GET /repos/{o}/{r}/pulls/{n}/requested_reviewers`.
//!
//! Currently-pending reviewer requests, separated into users and
//! teams. The endpoint returns richer user objects (avatar URLs,
//! `type`, etc.) and additional team fields; we model only what
//! downstream stages use.

use serde::{Deserialize, Serialize};

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug};

use super::gh::{GhError, gh_json};

/// Fetch currently-pending reviewer requests for a PR.
pub fn fetch_requested_reviewers(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<RequestedReviewers, GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/requested_reviewers");
    gh_json(&["api", &path])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct RequestedReviewers {
    #[serde(default)]
    pub users: Vec<RequestedUser>,
    #[serde(default)]
    pub teams: Vec<RequestedTeam>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RequestedUser {
    pub login: GitHubLogin,
    /// `User`, `Bot`, `Organization`, or `Mannequin`.
    #[serde(rename = "type")]
    pub user_type: UserType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum UserType {
    User,
    Bot,
    Organization,
    Mannequin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RequestedTeam {
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
    fn rejects_unknown_user_type() {
        let json = r#"{"users":[{"login":"x","type":"Martian"}],"teams":[]}"#;
        let err = serde_json::from_str::<RequestedReviewers>(json).unwrap_err();
        assert!(err.to_string().contains("Martian") || err.to_string().contains("unknown variant"));
    }
}
