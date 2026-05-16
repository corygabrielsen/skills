// Side effects for CI health remediation. Sibling to act/copilot.rs;
// future axes follow the same shape.

use crate::ids::RepoSlug;
use crate::observe::github::gh::{GhError, gh_run};
use crate::observe::github::workflow_runs::WorkflowRunId;

/// POST `/repos/:o/:r/actions/runs/:run_id/rerun` — re-runs every
/// job of the workflow run. The next observation iteration sees a
/// fresh `workflow_run` row whose `created_at` resets the per-(check,
/// HEAD) timer and whose `attempt_count` increments the budget.
pub fn rerun_workflow(slug: &RepoSlug, run_id: &WorkflowRunId) -> Result<(), GhError> {
    let path = format!("repos/{slug}/actions/runs/{run_id}/rerun");
    gh_run(&["api", &path, "--method", "POST"])
}
