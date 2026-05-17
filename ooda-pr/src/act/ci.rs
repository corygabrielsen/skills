//! Driver-side effects for the CI axis.

use crate::ids::RepoSlug;
use crate::observe::github::gh::{GhError, gh_run};
use crate::observe::github::workflow_runs::WorkflowRunId;

/// Re-run every job of a workflow run via the upstream actions
/// endpoint. The next observation cycle sees a fresh run row whose
/// timestamps reset the per-check / per-HEAD timing and whose
/// attempt count debits the remaining rerun budget.
pub(super) fn rerun_workflow(slug: &RepoSlug, run_id: &WorkflowRunId) -> Result<(), GhError> {
    let path = format!("repos/{slug}/actions/runs/{run_id}/rerun");
    gh_run(&["api", &path, "--method", "POST"])
}
