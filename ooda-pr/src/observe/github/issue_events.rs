//! Typed view of `GET /repos/{o}/{r}/issues/{n}/events` — the PR
//! timeline.
//!
//! Modeled as a flat struct: `event` is kept as a raw string so
//! downstream stages can classify it. `requested_reviewer` and
//! `requested_team` are only populated on
//! `review_requested` / `review_request_removed` events; other
//! events leave them null. Extra REST fields (id, node_id, url, …)
//! are ignored.

use serde::Deserialize;

use crate::ids::{GitHubLogin, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{gh_json_paginate, GhError};

/// Fetch PR timeline events. `gh api --paginate` emits one JSON
/// array per page; `gh_json_paginate` concatenates them.
pub fn fetch_issue_events(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<IssueEvent>, GhError> {
    let path = format!("repos/{slug}/issues/{pr}/events");
    gh_json_paginate(&["api", &path, "--paginate"])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueEvent {
    pub event: String,
    pub actor: Option<Actor>,
    /// Null for some system-originated events; REST will return `null`.
    pub created_at: Option<Timestamp>,
    #[serde(default)]
    pub requested_reviewer: Option<UserRef>,
    #[serde(default)]
    pub requested_team: Option<TeamRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Actor {
    pub login: GitHubLogin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UserRef {
    pub login: GitHubLogin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TeamRef {
    pub slug: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS_FIXTURE: &str =
        include_str!("../../../test/fixtures/github/issue_events.json");

    #[test]
    fn deserializes_full_fixture() {
        let events: Vec<IssueEvent> = serde_json::from_str(EVENTS_FIXTURE).unwrap();
        assert_eq!(events.len(), 13);
        // Every event has a non-empty name string.
        for e in &events {
            assert!(!e.event.is_empty());
        }
    }

    #[test]
    fn review_requested_with_team_has_requested_team() {
        let events: Vec<IssueEvent> = serde_json::from_str(EVENTS_FIXTURE).unwrap();
        let team_req = events
            .iter()
            .find(|e| e.event == "review_requested" && e.requested_team.is_some())
            .expect("fixture has a team-requested review");
        assert_eq!(team_req.requested_team.as_ref().unwrap().slug, "review-team");
        assert!(team_req.requested_reviewer.is_none());
    }

    #[test]
    fn review_requested_with_user_has_requested_reviewer() {
        let events: Vec<IssueEvent> = serde_json::from_str(EVENTS_FIXTURE).unwrap();
        let user_req = events
            .iter()
            .find(|e| e.event == "review_requested" && e.requested_reviewer.is_some())
            .expect("fixture has a user-requested review");
        assert_eq!(
            user_req.requested_reviewer.as_ref().unwrap().login.as_str(),
            "Copilot",
        );
        assert!(user_req.requested_team.is_none());
    }

    #[test]
    fn copilot_work_started_event_parses_with_no_extras() {
        let events: Vec<IssueEvent> = serde_json::from_str(EVENTS_FIXTURE).unwrap();
        let cws = events
            .iter()
            .find(|e| e.event == "copilot_work_started")
            .expect("fixture has copilot_work_started");
        assert!(cws.created_at.is_some());
        assert!(cws.requested_reviewer.is_none());
        assert!(cws.requested_team.is_none());
    }

    #[test]
    fn actor_can_be_null() {
        let json = r#"[
            {"event":"system_thing","actor":null,"created_at":null}
        ]"#;
        let events: Vec<IssueEvent> = serde_json::from_str(json).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].actor.is_none());
        assert!(events[0].created_at.is_none());
    }

    #[test]
    fn unknown_event_names_preserved_verbatim() {
        let json = r#"[
            {"event":"future_event_kind_not_yet_invented","actor":{"login":"a"},"created_at":"2026-04-23T00:00:00Z"}
        ]"#;
        let events: Vec<IssueEvent> = serde_json::from_str(json).unwrap();
        assert_eq!(events[0].event, "future_event_kind_not_yet_invented");
    }
}
