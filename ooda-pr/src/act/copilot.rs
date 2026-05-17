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
///
/// ## GitHub API for Copilot reviewer state
///
/// All facts below are empirically measured against an open PR
/// with Copilot configured as an auto-reviewer. A naive reading
/// of the GitHub REST docs predicts most of these wrong; do not
/// "fix" by switching to GraphQL `requestReviews` or
/// delete-and-re-add — neither is necessary and both have been
/// ruled out at design time.
///
/// - **POST slug**: `copilot-pull-request-reviewer[bot]` (the bot's
///   app slug with `[bot]` suffix). Stripping `[bot]` returns
///   HTTP 422 "Reviews may only be requested from collaborators".
/// - **GET login** (returned in `requested_reviewers[].login`):
///   `Copilot` (user-facing display name). The same reviewer is
///   addressable by three distinct surfaces — `Copilot`,
///   `copilot-pull-request-reviewer`, and
///   `copilot-pull-request-reviewer[bot]` — and the surface that
///   works depends on the verb.
/// - **DELETE slug**: `Copilot` (the GET login), **not** the POST
///   slug. Using the POST slug in DELETE returns HTTP 422
///   "Validation Failed". This asymmetry is non-obvious and not
///   documented by GitHub.
/// - **Idempotency**: repeat POST against pending state returns
///   2xx and does NOT generate a duplicate `review_requested`
///   timeline event. Safe to retry from the OODA loop without
///   inflating the orient layer's round count.
/// - **Timeline event**: each state-changing POST generates
///   exactly one `review_requested` issue event with
///   `requested_reviewer.login = "Copilot"`. The orient layer's
///   `correlate_rounds` depends on this event's `created_at`.
pub(super) fn rerequest_copilot(slug: &RepoSlug, pr: PullRequestNumber) -> Result<(), GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/requested_reviewers");
    let reviewer = format!("reviewers[]={COPILOT_REVIEWER_LOGIN}");
    gh_run(&["api", &path, "--method", "POST", "-f", &reviewer])
}
