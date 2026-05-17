//! Per-HEAD run-row source for CI health.
//!
//! # Invariants
//!
//! - **Health needs timing + attempts**: per-check health requires
//>   the per-run `created_at`/`run_started_at` anchors and per-(name,
//>   HEAD) attempt counts. The aggregated check projection drops
//>   both; this source is the only way to recover them.
//! - **Bounded N**: a single HEAD typically carries 0-30 rows; one
//!   page suffices.
//! - **No orient state**: counts are derivable from the wire shape
//!   alone — orient stays a pure projection.

use serde::{Deserialize, Serialize};

use crate::ids::{GitCommitSha, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json};

/// Stable run handle. Modeled as `u64` to match the wire shape and
/// the documented ID range; Display renders decimal so URL builders
/// embed the canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct WorkflowRunId(pub u64);

impl std::fmt::Display for WorkflowRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Disposition tag for a cancelled run. The host emits the same
/// wire value for auto-cancellation-by-concurrency and for genuine
/// terminal cancellation; this tag disambiguates them at the observe
/// boundary so health classification sees only the latter.
///
/// Variants are reserved for the disambiguation path; current
/// consumers do not construct them. Their presence locks the wire
/// schema against silent rename when the path comes online.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum CancelledDisposition {
    /// Auto-cancellation by concurrency — superseded by a newer run
    /// on the same HEAD. Health ignores these; they are normal push
    /// churn.
    Superseded,
    /// Genuine terminal cancellation. Health classifies as Failed.
    Terminal,
}

/// Run-level status. The Unknown catchall routes any future variant
/// to a known value so observation never aborts on unmodeled wire
/// shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowRunStatus {
    Queued,
    InProgress,
    Completed,
    Waiting,
    Requested,
    Pending,
    #[serde(other)]
    Unknown,
}

/// Run-level conclusion. Absent until the run completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowRunConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Neutral,
    Stale,
    StartupFailure,
    #[serde(other)]
    Unknown,
}

/// A single run row. Carries the identity and timing fields health
/// classification needs; richer fields are not modeled until a
/// consumer arrives.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct WorkflowRun {
    pub id: WorkflowRunId,
    /// Workflow name. Join key against the aggregated check
    /// projection at orient time.
    pub name: String,
    /// HEAD SHA the run was enqueued against. Per-(name, HEAD)
    /// attempt-budget scoping resets implicitly via SHA equality on
    /// HEAD movement.
    pub head_sha: GitCommitSha,
    pub status: WorkflowRunStatus,
    /// Absent until the run completes.
    #[serde(default)]
    pub conclusion: Option<WorkflowRunConclusion>,
    /// Enqueue time.
    pub created_at: Timestamp,
    /// Absent until the run begins executing.
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub run_started_at: Option<Timestamp>,
    /// 1-indexed re-run attempt counter on this row. The attempt
    /// budget is computed over distinct run rows by name on the
    /// current HEAD, not over this counter — re-runs of one row
    /// consume one budget slot.
    #[serde(default = "default_run_attempt")]
    pub run_attempt: u32,
}

fn default_run_attempt() -> u32 {
    1
}

fn deserialize_optional_timestamp<'de, D>(d: D) -> Result<Option<Timestamp>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Absence shapes (null, empty string) both decode to None so
    // downstream consumers handle them uniformly.
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => Timestamp::parse(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowRunsEnvelope {
    #[serde(default)]
    workflow_runs: Vec<WorkflowRun>,
}

/// Fetch every run row on the given HEAD. Bounded N — one page
/// suffices.
pub(crate) fn fetch_workflow_runs_for_head(
    slug: &RepoSlug,
    head_sha: &GitCommitSha,
) -> Result<Vec<WorkflowRun>, GhError> {
    let path = format!(
        "repos/{slug}/actions/runs?head_sha={}&per_page=100",
        head_sha.as_str(),
    );
    let env: WorkflowRunsEnvelope = gh_json(&["api", &path])?;
    Ok(env.workflow_runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha() -> GitCommitSha {
        GitCommitSha::parse(&"a".repeat(40)).unwrap()
    }

    #[test]
    fn deserializes_minimal_envelope() {
        let json = r#"{
            "workflow_runs": [{
                "id": 12345,
                "name": "CI",
                "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status": "in_progress",
                "conclusion": null,
                "created_at": "2026-05-16T10:00:00Z",
                "run_started_at": "2026-05-16T10:01:00Z",
                "run_attempt": 1
            }]
        }"#;
        let env: WorkflowRunsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.workflow_runs.len(), 1);
        let r = &env.workflow_runs[0];
        assert_eq!(r.id, WorkflowRunId(12345));
        assert_eq!(r.id.to_string(), "12345");
        assert_eq!(r.name, "CI");
        assert_eq!(r.head_sha, sha());
        assert_eq!(r.status, WorkflowRunStatus::InProgress);
        assert!(r.conclusion.is_none());
        assert!(r.run_started_at.is_some());
        assert_eq!(r.run_attempt, 1);
    }

    /// Regression for the reported wire-type bug. GitHub returns
    /// `workflow_run` `id` as a JSON integer (real value observed:
    /// 25961405250); the previous `String`-typed `WorkflowRunId`
    /// aborted observe with `invalid type: integer ..., expected a
    /// string` on every PR that had a workflow run, blocking the
    /// whole OODA pipeline with exit 70.
    #[test]
    fn integer_id_deserializes_without_type_error() {
        let json = r#"{
            "workflow_runs": [{
                "id": 25961405250,
                "name": "CI",
                "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status": "in_progress",
                "created_at": "2026-05-16T10:00:00Z"
            }]
        }"#;
        let env: WorkflowRunsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.workflow_runs[0].id, WorkflowRunId(25_961_405_250));
        assert_eq!(env.workflow_runs[0].id.to_string(), "25961405250");
    }

    #[test]
    fn empty_run_started_at_becomes_none() {
        let json = r#"{
            "workflow_runs": [{
                "id": 9,
                "name": "CI",
                "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status": "queued",
                "created_at": "2026-05-16T10:00:00Z",
                "run_started_at": ""
            }]
        }"#;
        let env: WorkflowRunsEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.workflow_runs[0].run_started_at.is_none());
        // run_attempt absent → default 1.
        assert_eq!(env.workflow_runs[0].run_attempt, 1);
    }

    #[test]
    fn unknown_status_routes_to_unknown_variant() {
        let json = r#"{
            "workflow_runs": [{
                "id": 1,
                "name": "x",
                "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status": "mystery",
                "created_at": "2026-05-16T10:00:00Z"
            }]
        }"#;
        let env: WorkflowRunsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.workflow_runs[0].status, WorkflowRunStatus::Unknown);
    }

    #[test]
    fn conclusion_action_required_parses() {
        // action_required is GHA's design state for manual approval;
        // observe should accept it on the wire even though the
        // health detector filters it out.
        let json = r#"{
            "workflow_runs": [{
                "id": 1,
                "name": "x",
                "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status": "completed",
                "conclusion": "action_required",
                "created_at": "2026-05-16T10:00:00Z"
            }]
        }"#;
        let env: WorkflowRunsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(
            env.workflow_runs[0].conclusion,
            Some(WorkflowRunConclusion::ActionRequired)
        );
    }

    // ── reserved CancelledDisposition variants ──
    //
    // `Superseded` and `Terminal` are reserved for the upcoming
    // Resolved-cancelled disambiguation path. No current code path
    // constructs them. Lock the wire names so the JSONL contract
    // stays stable as consumers come online.

    #[test]
    fn cancelled_disposition_superseded_serializes_to_pascal_case() {
        let json = serde_json::to_string(&CancelledDisposition::Superseded).unwrap();
        assert_eq!(json, "\"Superseded\"");
    }

    #[test]
    fn cancelled_disposition_terminal_serializes_to_pascal_case() {
        let json = serde_json::to_string(&CancelledDisposition::Terminal).unwrap();
        assert_eq!(json, "\"Terminal\"");
    }

    #[test]
    fn cancelled_disposition_distinguishes_variants() {
        assert_ne!(
            CancelledDisposition::Superseded,
            CancelledDisposition::Terminal,
        );
    }
}
