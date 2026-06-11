//! Aggregated check projection — one row per check name with its
//! latest state. Check-runs and status-check contexts are unified
//! into a single shape by the host CLI.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::ids::{CheckName, PullRequestNumber, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json_lenient};
use super::workflow_runs::WorkflowRunId;

const CHECK_FIELDS: &str = "name,state,description,link,completedAt";

/// Fetch every check (required and advisory) on a PR. Tolerant
/// decode lifts pending-but-otherwise-valid responses and absent-
/// checks responses into successful empty results.
///
/// Boundary-side confound filter: the manual-approval-pending state
/// is a design state, not a failure mode. Dropping it here keeps
/// the downstream classifier from masking it as a terminal failure
/// when it dominates per-day check noise.
pub(crate) fn fetch_pull_request_checks(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<PullRequestCheck>, GhError> {
    let slug_s = slug.to_string();
    let pr_s = pr.to_string();
    let raw: Vec<PullRequestCheck> = gh_json_lenient(
        &["pr", "checks", &pr_s, "-R", &slug_s, "--json", CHECK_FIELDS],
        Some((vec![], "no checks reported")),
    )?;
    Ok(raw
        .into_iter()
        .filter(|c| c.state != CheckState::ActionRequired)
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestCheck {
    pub name: CheckName,
    pub state: CheckState,
    /// One-line check title; often empty.
    #[serde(default)]
    pub description: String,
    /// URL to the run details page.
    #[serde(default)]
    pub link: String,
    /// Completion time; absent for not-yet-completed checks. Absence
    /// shapes (null, empty string) both decode to None.
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub completed_at: Option<Timestamp>,
}

/// Unified check state — host's conclusion and status vocabularies
/// merged into one enum. The Unknown catchall routes any future
/// variant to a known value so observation never aborts on
/// unmodeled wire shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum CheckState {
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
    // Absence shapes (null, empty string) both decode to None.
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => Timestamp::parse(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Extract the parent workflow run id from a check's `link` URL.
/// Workflow job URLs follow the structural form
/// `…/actions/runs/<run_id>/job/<job_id>`. The run id is not
/// exposed as a first-class field on any of the checks or rollup
/// endpoints; URL grammar is the platform contract. Non-Actions
/// checks (built-in gates, third-party apps) don't match the
/// grammar and return None.
#[must_use]
pub(crate) fn parse_run_id_from_link(url: &str) -> Option<WorkflowRunId> {
    let after_runs = url.split("/actions/runs/").nth(1)?;
    let mut segments = after_runs.split('/');
    let run_id_str = segments.next()?;
    if segments.next()? != "job" {
        return None;
    }
    let id: u64 = run_id_str.parse().ok()?;
    Some(WorkflowRunId(id))
}

/// Drop rollup rows whose parent workflow run was auto-cancelled by
/// a newer run on the same HEAD. These rows linger in the rollup
/// between cancellation and eviction and would otherwise surface as
/// transient noise (`TriageWait` / `FixCi` candidates) for a window
/// of ~15 minutes per cancellation event.
///
/// Genuinely terminal cancellations are NOT in the superseded set
/// and pass through unfiltered so CI health classifies them as
/// failed.
#[must_use]
pub(crate) fn filter_superseded_cancelled(
    checks: Vec<PullRequestCheck>,
    superseded_run_ids: &HashSet<WorkflowRunId>,
) -> Vec<PullRequestCheck> {
    if superseded_run_ids.is_empty() {
        return checks;
    }
    checks
        .into_iter()
        .filter(|c| match parse_run_id_from_link(c.link.as_str()) {
            Some(run_id) => !superseded_run_ids.contains(&run_id),
            None => true,
        })
        .collect()
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
    fn action_required_filters_out_at_observe_boundary() {
        // ACTION_REQUIRED is GHA's design state for manual approval —
        // "AI: Claude Code" emits dozens per day per repo. Letting it
        // reach orient would mask as a terminal failure (current
        // classify_into routes it to the failed bucket). Filter at
        // the boundary so the CI orient projection never sees it.
        let json = r#"[
            {"name":"a","state":"SUCCESS","description":"","link":"","completedAt":""},
            {"name":"AI: Claude Code","state":"ACTION_REQUIRED","description":"","link":"","completedAt":""},
            {"name":"b","state":"FAILURE","description":"","link":"","completedAt":""}
        ]"#;
        // Exercise the filter via the same path callers use: parse +
        // filter, matching the body of `fetch_pull_request_checks` after the
        // `gh_json_lenient` call. (We can't call fetch_pull_request_checks
        // directly without spawning `gh`.)
        let raw: Vec<PullRequestCheck> = serde_json::from_str(json).unwrap();
        let filtered: Vec<PullRequestCheck> = raw
            .into_iter()
            .filter(|c| c.state != CheckState::ActionRequired)
            .collect();
        assert_eq!(filtered.len(), 2);
        assert!(
            !filtered
                .iter()
                .any(|c| c.state == CheckState::ActionRequired)
        );
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

    fn check(name: &str, state: CheckState, link: &str) -> PullRequestCheck {
        PullRequestCheck {
            name: CheckName::parse(name).unwrap(),
            state,
            description: String::new(),
            link: link.to_string(),
            completed_at: None,
        }
    }

    #[test]
    fn parse_run_id_from_link_well_formed_actions_url() {
        let url = "https://github.com/w3-io/w3/actions/runs/27206702939/job/80324317269";
        assert_eq!(
            parse_run_id_from_link(url),
            Some(WorkflowRunId(27_206_702_939)),
        );
    }

    #[test]
    fn parse_run_id_from_link_non_actions_url_returns_none() {
        // Third-party check (Cursor Bugbot) — URL grammar doesn't
        // match; passes through.
        assert_eq!(
            parse_run_id_from_link("https://cursor.com/docs/bugbot"),
            None
        );
    }

    #[test]
    fn parse_run_id_from_link_empty_url_returns_none() {
        assert_eq!(parse_run_id_from_link(""), None);
    }

    #[test]
    fn parse_run_id_from_link_actions_url_without_job_returns_none() {
        // `actions/runs/N` without the `/job/M` suffix is the
        // run-summary URL, not a per-job rollup row. We only want
        // to map per-job rows back to runs.
        let url = "https://github.com/w3-io/w3/actions/runs/27206702939";
        assert_eq!(parse_run_id_from_link(url), None);
    }

    #[test]
    fn parse_run_id_from_link_non_numeric_run_id_returns_none() {
        let url = "https://github.com/w3-io/w3/actions/runs/abc/job/123";
        assert_eq!(parse_run_id_from_link(url), None);
    }

    #[test]
    fn filter_superseded_cancelled_drops_superseded_parent_rows() {
        let superseded: HashSet<WorkflowRunId> =
            [WorkflowRunId(27_206_702_939)].into_iter().collect();
        let checks = vec![
            check(
                "Cancel Orphaned Runs",
                CheckState::Cancelled,
                "https://github.com/o/r/actions/runs/27206702939/job/80324317269",
            ),
            check(
                "Build",
                CheckState::Success,
                "https://github.com/o/r/actions/runs/27206692141/job/80324300000",
            ),
        ];
        let filtered = filter_superseded_cancelled(checks, &superseded);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name.as_str(), "Build");
    }

    #[test]
    fn filter_superseded_cancelled_keeps_terminal_cancelled_rows() {
        // Terminal cancellation: parent run is NOT in the superseded
        // set. The CANCELLED row passes through so CI health can
        // classify it as Failed.
        let superseded: HashSet<WorkflowRunId> = HashSet::new();
        let checks = vec![check(
            "User-cancelled",
            CheckState::Cancelled,
            "https://github.com/o/r/actions/runs/12345/job/67890",
        )];
        let filtered = filter_superseded_cancelled(checks, &superseded);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_superseded_cancelled_keeps_non_actions_checks() {
        // Built-in synthetic gates (Mergeability Check) and third-
        // party app checks have URLs that don't match the actions
        // grammar. They pass through unconditionally.
        let superseded: HashSet<WorkflowRunId> =
            [WorkflowRunId(27_206_702_939)].into_iter().collect();
        let checks = vec![
            check("Mergeability Check", CheckState::Failure, ""),
            check(
                "Cursor Bugbot",
                CheckState::Success,
                "https://cursor.com/docs/bugbot",
            ),
        ];
        let filtered = filter_superseded_cancelled(checks, &superseded);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_superseded_cancelled_empty_set_is_identity() {
        // Hot path: no cancelled runs → no work.
        let superseded: HashSet<WorkflowRunId> = HashSet::new();
        let checks = vec![
            check("a", CheckState::Success, "u"),
            check("b", CheckState::Failure, "v"),
        ];
        let filtered = filter_superseded_cancelled(checks.clone(), &superseded);
        assert_eq!(filtered.len(), checks.len());
    }
}
