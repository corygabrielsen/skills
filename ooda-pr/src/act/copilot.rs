//! Driver-side effects for the bot-review axis.

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::orient::copilot::COPILOT_REVIEWER_LOGIN;

/// Re-request the bot as a reviewer via the upstream REST surface.
///
/// The driver-side effect is identical for the health-remediation
/// and tier-advancement paths; the symptom carried on the action
/// only affects the blocker key so the stall comparator separates
/// the cases.
///
/// # Empirically verified upstream contract
///
/// The upstream reviewer endpoint for this bot has three
/// non-obvious properties that any change to this call site must
/// preserve:
///
/// - **Verb-asymmetric identity**. The POST and DELETE verbs
///   address the same reviewer by different identifiers; using
///   one identifier on the wrong verb returns a 422. Switching
///   to a unified identifier is not an option at this layer.
/// - **Idempotent re-POST**. A repeat POST against pending state
///   succeeds without emitting a duplicate timeline event, so
///   retries from the loop do not inflate the per-PR round count
///   observed by the orient layer.
/// - **Exactly-one-event-per-state-change**. Each state-changing
///   POST produces exactly one timeline event; the orient layer
///   correlates rounds against the event's timestamp.
pub(super) fn rerequest_copilot(slug: &RepoSlug, pr: PullRequestNumber) -> Result<(), GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/requested_reviewers");
    let reviewer = format!("reviewers[]={COPILOT_REVIEWER_LOGIN}");
    gh_run(&["api", &path, "--method", "POST", "-f", &reviewer])
}
