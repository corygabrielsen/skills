//! Typed view of `GET /repos/{o}/{r}/actions/runs?head_sha={sha}`.
//!
//! `gh pr checks` surfaces only the latest state per check name —
//! no `run_id`, no `created_at`, no `run_started_at`, no attempt
//! history. The CI health detector needs all three for per-check
//! timing and per-(name, HEAD) attempt counting. Fetch the workflow
//! runs scoped to the current HEAD here; orient joins by workflow
//! name + head_sha at decide time.
//
// gh pr checks does not surface created_at/run_started_at; fetch via
// /actions/runs?head_sha=. Bounded N (only runs on current HEAD,
// typically 0-30). Preserves orient's pure-projection invariant —
// no hidden state.

use serde::{Deserialize, Serialize};

use crate::ids::{GitCommitSha, RepoSlug, Timestamp};

use super::gh::{GhError, gh_json};

/// Stable handle for a workflow run. GitHub Actions returns this as
/// a JSON integer on the wire (`/repos/:o/:r/actions/runs`); modeling
/// it as `String` aborted observe with a `serde` type error on every
/// PR that had any workflow runs. `u64` matches the wire shape and
/// the documented ID range; Display formats as decimal so the act-
/// stage URL builder (`/actions/runs/{run_id}/rerun`) stays correct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct WorkflowRunId(pub u64);

impl std::fmt::Display for WorkflowRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Disposition tag for a cancelled workflow run. GitHub Actions
/// emits `cancelled` for auto-cancel-by-concurrency (PR superseded
/// by newer push) AND for genuine outage-cancels. Distinguish at
/// the observe boundary so the health detector sees only the latter.
//
// v1 heuristic: a cancelled run is `Superseded` if a strictly newer
// run (greater run_attempt OR newer created_at) for the same workflow
// name exists on the same head_sha. Otherwise `Terminal`. The full
// "superseded by a newer PUSH" detection lives in a future
// cross-SHA join — see workflow_runs spec note.
//
// Reserved for the upcoming Resolved-cancelled disambiguation path —
// no consumer in v1 because cancelled runs route through CiSummary's
// existing failed bucket (preserves prior decide behavior). The
// type exists so the wire model is correctly shaped at the boundary;
// a future iteration wires the disposition into orient/ci.rs.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CancelledDisposition {
    /// Auto-cancel-by-concurrency: another run on the same head_sha
    /// supersedes this one. The health detector ignores these — they
    /// represent normal developer push churn, not a failure mode.
    Superseded,
    /// Genuine terminal cancellation. Either GitHub Actions outage,
    /// manual cancel, or a workflow-level failure. The health detector
    /// treats these as terminal Failed.
    Terminal,
}

/// Wire status of a workflow run. Mirrors GitHub's vocabulary; an
/// unknown future variant routes to `Unknown` for forward
/// compatibility (same pattern as `CheckState::Unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Queued,
    InProgress,
    Completed,
    Waiting,
    Requested,
    Pending,
    #[serde(other)]
    Unknown,
}

/// Wire conclusion of a completed workflow run. `None` when the run
/// has not completed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunConclusion {
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

/// A single workflow run. Carries the timing and identity fields
/// the CI health detector needs; richer fields (URLs, actor, etc.)
/// are deliberately not modeled — add them only when a consumer
/// arrives.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    /// Workflow display name. Joined against `PullRequestCheck.name`
    /// at orient time.
    pub name: String,
    /// HEAD commit SHA the run was enqueued against. Used to scope
    /// the per-(name, HEAD) budget — force-push to a new HEAD
    /// implicitly resets the budget via SHA equality.
    pub head_sha: GitCommitSha,
    pub status: WorkflowRunStatus,
    /// `None` until the run completes.
    #[serde(default)]
    pub conclusion: Option<WorkflowRunConclusion>,
    /// Enqueue time (when the run row was created in GHA).
    pub created_at: Timestamp,
    /// `None` when the run has not yet started (still queued).
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub run_started_at: Option<Timestamp>,
    /// 1-indexed attempt counter; increments on re-runs of the same
    /// underlying run row. The per-(name, HEAD) budget compares the
    /// COUNT of distinct workflow runs by name on a given HEAD, not
    /// this attempt counter — re-runs of one row still consume one
    /// slot in the budget.
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
    // The API emits null when the run has not started; some clients
    // (and replay fixtures) emit "" instead. Normalise both to None.
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

/// Fetch every workflow run on `head_sha` for `slug`. The set is
/// bounded — typically 0-30 rows on a single commit — so a single
/// page suffices (per_page=100).
pub fn fetch_workflow_runs_for_head(
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
    /// workflow_run `id` as a JSON integer (real value observed:
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
}
