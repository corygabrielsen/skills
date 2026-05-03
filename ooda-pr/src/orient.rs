//! Orient stage: project raw observations into typed axes that
//! decide consumes. One module per axis, building bottom-up.
//! The shared shape (Facts / Findings / Opportunities, etc.) emerges
//! once a second axis lands and forces the abstraction.

pub mod bot_threads;
pub mod ci;
pub mod copilot;
pub mod cursor;
pub mod required_checks;
pub mod reviews;
pub mod state;

use crate::ids::Timestamp;
use crate::observe::github::GitHubObservations;

use ci::CiSummary;
use copilot::{orient_copilot, CopilotRepoConfig, CopilotReport};
use cursor::{orient_cursor, CursorReport};
use reviews::ReviewSummary;
use state::PullRequestState;

/// All five orient axes assembled from a single observation bundle.
///
/// No combined "score" or "tier" — those are derived display values
/// that decide computes on demand. The struct is per-axis so adding
/// a sixth (e.g. codex) is purely additive.
///
/// **Asymmetric optionality is intentional.** `ci`, `state`, and
/// `reviews` are always-present (every PR has CI buckets, lifecycle
/// state, and a review summary — possibly empty). `copilot` and
/// `cursor` are `Option` because absence of bot signal is
/// *structurally distinct* from low signal — a repo without
/// Copilot configured (`None`) must not be treated the same as a
/// repo with Copilot configured but dormant on this PR
/// (`Some(report)` with `activity = Idle`). The old combined-score
/// approach conflated these and produced false halts; the
/// per-axis `Option` makes the distinction unrepresentable.
#[derive(Debug, Clone)]
pub struct OrientedState {
    pub ci: CiSummary,
    pub state: PullRequestState,
    pub reviews: ReviewSummary,
    /// `None` when Copilot is not configured for the repo (no
    /// active `copilot_code_review` ruleset rule). Distinct from
    /// `Some(report)` with `activity = Idle` (configured but not
    /// engaged on this PR).
    pub copilot: Option<CopilotReport>,
    /// `None` when no Cursor activity exists for this PR (no rounds
    /// and no Bugbot check). Activity-gated, not config-gated —
    /// Cursor has no equivalent of a ruleset config endpoint, so
    /// "configured" is observable only via activity.
    pub cursor: Option<CursorReport>,
}

/// Compose all axes from a single GitHub observation bundle.
///
/// `last_commit_at` comes from outside the GitHub fetch bundle
/// (typically `git log` on HEAD); pass `None` if unavailable.
pub fn orient(
    obs: &GitHubObservations,
    last_commit_at: Option<Timestamp>,
) -> OrientedState {
    let required = required_checks::required_check_names(
        &obs.branch_rules,
        obs.branch_protection.as_ref(),
    );
    let ci = ci::orient_ci(&obs.checks, &required);
    let pr_state = state::orient_state(&obs.pr_view, last_commit_at);
    let reviews = reviews::orient_reviews(
        &obs.pr_view,
        &obs.review_threads_page,
        &obs.issue_comments,
        &obs.reviews,
    );
    let copilot = obs.copilot_config.map(CopilotRepoConfig::from).and_then(|cfg| {
        orient_copilot(
            cfg,
            &obs.issue_events,
            &obs.reviews,
            &obs.review_threads_page,
            &obs.requested_reviewers,
            &obs.pr_view.head_ref_oid,
        )
    });
    let cursor = orient_cursor(
        &obs.reviews,
        &obs.review_threads_page,
        &obs.checks,
        &obs.pr_view.head_ref_oid,
    );

    OrientedState {
        ci,
        state: pr_state,
        reviews,
        copilot,
        cursor,
    }
}
