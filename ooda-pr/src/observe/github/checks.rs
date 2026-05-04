//! Typed view of `gh pr checks --json name,state,description,link,completedAt`.
//!
//! Check runs and status-check contexts are unified by the `gh` CLI
//! into a single flat array. Each entry is one check with its latest
//! state and a link to the run detail page.

use serde::{Deserialize, Deserializer, Serialize};

use crate::ids::{CheckName, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json_lenient};

const CHECK_FIELDS: &str = "name,state,description,link,completedAt";

/// Fetch all check runs on a PR (required + optional) via
/// `gh pr checks`. Uses lenient parse:
///   - status 8 with valid JSON → checks still pending (parse it)
///   - status 1 with empty stdout → "no checks reported" → `vec![]`
pub fn fetch_pr_checks(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<PullRequestCheck>, GhError> {
    let slug_s = slug.to_string();
    let pr_s = pr.to_string();
    gh_json_lenient(
        &["pr", "checks", &pr_s, "-R", &slug_s, "--json", CHECK_FIELDS],
        Some((vec![], "no checks reported")),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCheck {
    pub name: CheckName,
    pub state: CheckState,
    /// Check output title (one-liner). Often empty.
    #[serde(default)]
    pub description: String,
    /// URL to the check run details page.
    #[serde(default)]
    pub link: String,
    /// ISO-8601 completion time. `gh` emits `""` for checks that have
    /// not completed yet; this is normalised to `None`.
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub completed_at: Option<Timestamp>,
}

/// GitHub's check-run conclusion + status vocabulary, mapped to a
/// single enum. Variants beyond the common six (Success / Failure /
/// Skipped / Neutral / InProgress / Queued) cover gh's full output:
/// Cancelled, TimedOut, ActionRequired, Stale, Pending all show up
/// in real PRs. An unknown variant we haven't seen yet routes to
/// `Unknown` so observe doesn't abort on a future addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckState {
    Success,
    Failure,
    Skipped,
    Neutral,
    InProgress,
    Queued,
    Pending,
    Cancelled,
    TimedOut,
    ActionRequired,
    Stale,
    StartupFailure,
    #[serde(other)]
    Unknown,
}

fn deserialize_optional_timestamp<'de, D>(d: D) -> Result<Option<Timestamp>, D::Error>
where
    D: Deserializer<'de>,
{
    // gh emits "" for not-yet-completed runs instead of null.
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => Timestamp::parse(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKS_FIXTURE: &str = include_str!("../../../test/fixtures/github/pr_checks.json");

    #[test]
    fn deserializes_full_fixture() {
        let checks: Vec<PullRequestCheck> = serde_json::from_str(CHECKS_FIXTURE).unwrap();
        assert_eq!(checks.len(), 32);
        // Every check has a non-empty name and a valid state.
        for c in &checks {
            assert!(!c.name.as_str().is_empty(), "name empty: {c:?}");
        }
    }

    #[test]
    fn state_counts_match_fixture() {
        let checks: Vec<PullRequestCheck> = serde_json::from_str(CHECKS_FIXTURE).unwrap();
        let n_success = checks
            .iter()
            .filter(|c| c.state == CheckState::Success)
            .count();
        let n_skipped = checks
            .iter()
            .filter(|c| c.state == CheckState::Skipped)
            .count();
        // This was a fully merged PR with no failures/in-progress.
        assert_eq!(n_success + n_skipped, checks.len());
        assert_eq!(
            checks
                .iter()
                .filter(|c| matches!(
                    c.state,
                    CheckState::Failure | CheckState::InProgress | CheckState::Queued
                ))
                .count(),
            0,
        );
    }

    #[test]
    fn empty_completed_at_becomes_none() {
        let json = r#"[{
            "name": "Pending Build",
            "state": "IN_PROGRESS",
            "description": "",
            "link": "",
            "completedAt": ""
        }]"#;
        let checks: Vec<PullRequestCheck> = serde_json::from_str(json).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].state, CheckState::InProgress);
        assert_eq!(checks[0].completed_at, None);
    }

    #[test]
    fn all_six_states_round_trip() {
        let json = r#"[
            {"name":"a","state":"SUCCESS","description":"","link":"","completedAt":""},
            {"name":"b","state":"FAILURE","description":"","link":"","completedAt":""},
            {"name":"c","state":"SKIPPED","description":"","link":"","completedAt":""},
            {"name":"d","state":"NEUTRAL","description":"","link":"","completedAt":""},
            {"name":"e","state":"IN_PROGRESS","description":"","link":"","completedAt":""},
            {"name":"f","state":"QUEUED","description":"","link":"","completedAt":""}
        ]"#;
        let checks: Vec<PullRequestCheck> = serde_json::from_str(json).unwrap();
        let states: Vec<_> = checks.iter().map(|c| c.state).collect();
        assert_eq!(
            states,
            vec![
                CheckState::Success,
                CheckState::Failure,
                CheckState::Skipped,
                CheckState::Neutral,
                CheckState::InProgress,
                CheckState::Queued,
            ],
        );
    }

    #[test]
    fn unknown_state_routes_to_unknown_variant() {
        // Forward-compatible: a future GitHub state we haven't
        // modeled yet shouldn't abort the whole observe phase.
        let json =
            r#"[{"name":"x","state":"MYSTERY","description":"","link":"","completedAt":""}]"#;
        let checks: Vec<PullRequestCheck> = serde_json::from_str(json).unwrap();
        assert_eq!(checks[0].state, CheckState::Unknown);
    }

    #[test]
    fn cancelled_and_timed_out_states_parse() {
        for s in [
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STALE",
            "PENDING",
        ] {
            let json = format!(
                r#"[{{"name":"x","state":"{s}","description":"","link":"","completedAt":""}}]"#,
            );
            let checks: Vec<PullRequestCheck> = serde_json::from_str(&json).unwrap();
            assert_ne!(
                checks[0].state,
                CheckState::Unknown,
                "{s} should parse to a typed variant"
            );
        }
    }
}
