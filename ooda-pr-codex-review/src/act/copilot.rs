// Side effects for Copilot health remediation. Sibling act/ci.rs
// etc. will follow.

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::copilot::COPILOT_REVIEWER_LOGIN;

/// Re-request Copilot as a reviewer on the PR via REST. Used by both
/// the tier-advancement path (`symptom = None`) and the
/// health-remediation path (`symptom = Some(_)`); the side effect is
/// identical — the symptom only travels in the action's blocker tag
/// so the stall comparator separates the cases.
pub(super) fn rerequest_copilot(slug: &RepoSlug, pr: PullRequestNumber) -> Result<(), GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/requested_reviewers");
    let reviewer = format!("reviewers[]={COPILOT_REVIEWER_LOGIN}");
    gh_run(&["api", &path, "--method", "POST", "-f", &reviewer])
}
