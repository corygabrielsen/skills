//! Typed view of the Cursor Bugbot `check_suite` + child `check_run` on
//! the current HEAD.
//!
//! Two endpoints, fused at observe boundary:
//!   1. `GET /repos/{o}/{r}/commits/{sha}/check-suites` filtered by
//!      `app.slug == "cursor"` — needed to detect the canonical
//!      Cursor stall (a `queued` `check_suite` that never spawns a
//!      child `check_run`). `gh pr checks` aggregates by check name
//!      and cannot see this state — the suite has no name.
//!   2. `GET /repos/{o}/{r}/commits/{sha}/check-runs?check_name=Cursor%20Bugbot`
//!      — once the suite spawns a run, this carries the per-run
//!      `started_at` (the per-run pickup anchor) that
//!      `PullRequestCheck` does not surface.
//!
//! The orient layer fuses these into the Cursor activity state. If
//! the suite is absent, Cursor is not active for the current HEAD
//! (or the PR's author was filtered server-side — that distinction
//! lives in the Cursor orient axis, not here).

use serde::{Deserialize, Serialize};

use crate::ids::{GitCommitSha, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json};

/// GitHub App slug for Cursor. Single canonical value; Cursor ships
/// exactly one app (no `cursor-swe-agent` style confound — see
/// `feedback-domain-shapes-design`).
const CURSOR_APP_SLUG: &str = "cursor";

/// Display name of Cursor's `check_run`. Stable in their docs;
/// reproduced here for the `check_name=` query filter.
const CURSOR_CHECK_RUN_NAME: &str = "Cursor Bugbot";

// Wire shapes for `/commits/{sha}/check-suites`. The endpoint returns
// `{ total_count, check_suites: [...] }`; we deserialize only the
// fields the Cursor activity classifier consumes.

#[derive(Debug, Clone, Deserialize)]
struct CheckSuitesEnvelope {
    #[serde(default)]
    check_suites: Vec<CheckSuiteWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct CheckSuiteWire {
    status: CheckSuiteStatus,
    created_at: Timestamp,
    app: Option<AppRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct AppRef {
    slug: String,
}

// Wire shapes for `/commits/{sha}/check-runs?check_name=`.

#[derive(Debug, Clone, Deserialize)]
struct CheckRunsEnvelope {
    #[serde(default)]
    check_runs: Vec<CheckRunWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct CheckRunWire {
    status: CheckRunStatus,
    #[serde(default)]
    conclusion: Option<CheckRunConclusion>,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    started_at: Option<Timestamp>,
}

/// `status` field on a `check_suite`. Cursor's stall signature is a
/// suite stuck in `Queued` with no child `check_run` ever appearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckSuiteStatus {
    Queued,
    InProgress,
    Completed,
    #[serde(other)]
    Unknown,
}

/// `status` field on a `check_run`. Modeled separately from the
/// `check_suite` status because the two carry semantically different
/// state — a `completed` suite can host a `queued` run (rare) or
/// vice versa during eventual-consistency windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRunStatus {
    Queued,
    InProgress,
    Completed,
    Pending,
    #[serde(other)]
    Unknown,
}

/// `conclusion` field on a `completed` `check_run`. Modeled separately
/// from `CheckState::Conclusion`/`WorkflowRunConclusion` because
/// Cursor's neutral disambiguation logic reads this as a domain
/// signal — not just a pass/fail bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRunConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    TimedOut,
    Skipped,
    ActionRequired,
    Stale,
    StartupFailure,
    #[serde(other)]
    Unknown,
}

/// Fused Cursor signal on the current HEAD. `None` for `suite` means
/// Cursor has not opened a `check_suite` for this commit — the orient
/// axis interprets that against the PR author to distinguish
/// "Cursor declined this PR" from "Cursor not active in this repo".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorStatus {
    /// `None` when Cursor has not opened a `check_suite` on this HEAD.
    pub suite: Option<CursorCheckSuite>,
    /// `None` when no child `check_run` exists. The suite may still
    /// exist (this is the canonical stall pattern). At most one run
    /// per suite — Cursor produces a single Bugbot run, not a fan-out.
    pub run: Option<CursorCheckRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorCheckSuite {
    pub status: CheckSuiteStatus,
    /// Suite creation timestamp — when GitHub received the push
    /// webhook for Cursor. Anchor for the `no child run` stall
    /// detector: `now - created_at >= STALL_TIMEOUT` with no child
    /// run is the canonical stuck-suite signature.
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CursorCheckRun {
    pub status: CheckRunStatus,
    pub conclusion: Option<CheckRunConclusion>,
    /// `None` when the run was created but never started (extremely
    /// rare; the orient axis falls back to the suite's `created_at`).
    pub started_at: Option<Timestamp>,
}

/// Fetch the Cursor `check_suite` and `check_run` (if any) for `head`.
/// Two REST calls in series — the suite alone tells us "is Cursor
/// active at all on this HEAD"; the run carries the per-run timing
/// when present. Bounded N (≤1 suite, ≤1 run per suite for Cursor).
pub fn fetch_cursor_status(slug: &RepoSlug, head: &GitCommitSha) -> Result<CursorStatus, GhError> {
    let suite = fetch_cursor_check_suite(slug, head)?;
    // Skip the check_runs call when no Cursor suite exists — the run
    // can't exist without a suite, and the second REST call would
    // just return an empty list.
    let run = if suite.is_some() {
        fetch_cursor_check_run(slug, head)?
    } else {
        None
    };
    Ok(CursorStatus { suite, run })
}

fn fetch_cursor_check_suite(
    slug: &RepoSlug,
    head: &GitCommitSha,
) -> Result<Option<CursorCheckSuite>, GhError> {
    let path = format!(
        "repos/{slug}/commits/{}/check-suites?per_page=100",
        head.as_str(),
    );
    let env: CheckSuitesEnvelope = gh_json(&["api", &path])?;
    Ok(env
        .check_suites
        .into_iter()
        .find(|s| s.app.as_ref().is_some_and(|a| a.slug == CURSOR_APP_SLUG))
        .map(|s| CursorCheckSuite {
            status: s.status,
            created_at: s.created_at,
        }))
}

fn fetch_cursor_check_run(
    slug: &RepoSlug,
    head: &GitCommitSha,
) -> Result<Option<CursorCheckRun>, GhError> {
    // `check_name=` filters server-side. URL-encode the space.
    let path = format!(
        "repos/{slug}/commits/{}/check-runs?check_name={}&per_page=10",
        head.as_str(),
        CURSOR_CHECK_RUN_NAME.replace(' ', "%20"),
    );
    let env: CheckRunsEnvelope = gh_json(&["api", &path])?;
    // Cursor produces exactly one check_run per suite. If multiple
    // somehow appear (e.g. a re-run that didn't replace the prior
    // row), take the most recently started one — the latest run's
    // status reflects the current Cursor state.
    Ok(env
        .check_runs
        .into_iter()
        .max_by_key(|r| r.started_at)
        .map(|r| CursorCheckRun {
            status: r.status,
            conclusion: r.conclusion,
            started_at: r.started_at,
        }))
}

fn deserialize_optional_timestamp<'de, D>(d: D) -> Result<Option<Timestamp>, D::Error>
where
    D: serde::Deserializer<'de>,
{
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

    #[test]
    fn check_suites_envelope_parses_cursor_app() {
        let json = r#"{
            "total_count": 2,
            "check_suites": [
                {"status":"completed","created_at":"2026-04-23T10:00:00Z","app":{"slug":"other"}},
                {"status":"queued","created_at":"2026-04-23T10:05:00Z","app":{"slug":"cursor"}}
            ]
        }"#;
        let env: CheckSuitesEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.check_suites.len(), 2);
        let cursor = env
            .check_suites
            .iter()
            .find(|s| s.app.as_ref().is_some_and(|a| a.slug == CURSOR_APP_SLUG))
            .unwrap();
        assert_eq!(cursor.status, CheckSuiteStatus::Queued);
    }

    #[test]
    fn check_runs_envelope_parses_completed_neutral() {
        let json = r#"{
            "total_count": 1,
            "check_runs": [
                {
                    "status": "completed",
                    "conclusion": "neutral",
                    "started_at": "2026-04-23T10:01:00Z"
                }
            ]
        }"#;
        let env: CheckRunsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.check_runs[0].status, CheckRunStatus::Completed);
        assert_eq!(
            env.check_runs[0].conclusion,
            Some(CheckRunConclusion::Neutral),
        );
        assert!(env.check_runs[0].started_at.is_some());
    }

    #[test]
    fn check_run_started_at_empty_string_parses_as_none() {
        let json = r#"{
            "total_count": 1,
            "check_runs": [
                {"status":"queued","conclusion":null,"started_at":""}
            ]
        }"#;
        let env: CheckRunsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.check_runs[0].started_at, None);
    }

    #[test]
    fn unknown_status_routes_to_unknown_variant() {
        // Forward-compat: a future Cursor or GitHub state we have not
        // modeled must not abort observe.
        let json = r#"{
            "total_count": 1,
            "check_suites": [
                {"status":"mystery","created_at":"2026-04-23T10:00:00Z","app":{"slug":"cursor"}}
            ]
        }"#;
        let env: CheckSuitesEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.check_suites[0].status, CheckSuiteStatus::Unknown);
    }

    #[test]
    fn cursor_suite_absent_when_only_other_apps() {
        let json = r#"{
            "total_count": 2,
            "check_suites": [
                {"status":"completed","created_at":"2026-04-23T10:00:00Z","app":{"slug":"github-actions"}},
                {"status":"in_progress","created_at":"2026-04-23T10:05:00Z","app":{"slug":"copilot"}}
            ]
        }"#;
        let env: CheckSuitesEnvelope = serde_json::from_str(json).unwrap();
        let cursor = env
            .check_suites
            .iter()
            .find(|s| s.app.as_ref().is_some_and(|a| a.slug == CURSOR_APP_SLUG));
        assert!(cursor.is_none());
    }
}
