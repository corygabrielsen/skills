//! Per-HEAD suite + run signal for the push-driven reviewer.
//!
//! # Invariants
//!
//! - **Two endpoints, fused at the boundary**: the canonical stall
//!   has no child run and is therefore invisible to name-filtered
//!   check observation; only the suite endpoint witnesses it. The
//!   run endpoint adds per-run timing once the suite spawns a run.
//! - **Suite-presence gates run-fetch**: a run cannot exist without
//!   a suite; the run call is skipped when no suite is observed.
//! - **At most one run per suite for this reviewer**: when multiple
//!   appear during eventual-consistency or re-run windows, the
//!   most-recently-started row wins.

use serde::{Deserialize, Serialize};

use crate::ids::{GitCommitSha, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json};

/// Originating-app slug. Single canonical value (no aliasing
/// confound across distinct apps).
const CURSOR_APP_SLUG: &str = "cursor";

/// Run display name used for server-side filtering.
const CURSOR_CHECK_RUN_NAME: &str = "Cursor Bugbot";

// Wire shapes for the suite endpoint. Only the fields the activity
// classifier consumes are decoded.

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

// Wire shapes for the run endpoint.

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

/// Suite status. The canonical stall is a long-Queued suite with no
/// child run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckSuiteStatus {
    Queued,
    InProgress,
    Completed,
    #[serde(other)]
    Unknown,
}

/// Run status. Modeled separately from suite status because the two
/// carry distinct semantics and can disagree during eventual-
/// consistency windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckRunStatus {
    Queued,
    InProgress,
    Completed,
    Pending,
    #[serde(other)]
    Unknown,
}

/// Run conclusion. Modeled separately from sibling-axis conclusion
/// enums because the disambiguation logic reads these values as
/// domain signals, not just pass/fail buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckRunConclusion {
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

/// Fused per-HEAD signal. Absent suite means no engagement on this
/// HEAD; the orient axis disambiguates against the PR author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CursorStatus {
    /// Absent when no suite opened on this HEAD.
    pub suite: Option<CursorCheckSuite>,
    /// Absent when no child run exists. Suite without run is the
    /// canonical stall pattern.
    pub run: Option<CursorCheckRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CursorCheckSuite {
    pub status: CheckSuiteStatus,
    /// Suite-creation anchor for the stuck-suite detector.
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CursorCheckRun {
    pub status: CheckRunStatus,
    pub conclusion: Option<CheckRunConclusion>,
    /// Absent when the run was created but never started. The
    /// orient layer falls back to the suite anchor.
    pub started_at: Option<Timestamp>,
}

/// Fetch the suite (and optional run) for the given HEAD. Two
/// serial calls — the suite witnesses engagement, the run carries
/// per-run timing. Bounded N (≤1 suite, ≤1 run per suite).
pub(crate) fn fetch_cursor_status(
    slug: &RepoSlug,
    head: &GitCommitSha,
) -> Result<CursorStatus, GhError> {
    let suite = fetch_cursor_check_suite(slug, head)?;
    // Suite-presence gates run-fetch — see module-level invariant.
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
    // Server-side name filter; the host requires whitespace URL-
    // encoding inside query values.
    let path = format!(
        "repos/{slug}/commits/{}/check-runs?check_name={}&per_page=10",
        head.as_str(),
        CURSOR_CHECK_RUN_NAME.replace(' ', "%20"),
    );
    let env: CheckRunsEnvelope = gh_json(&["api", &path])?;
    // At most one run per suite for this reviewer — see module
    // invariant. Most-recently-started wins when multiple appear.
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
