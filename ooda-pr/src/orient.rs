//! Orient stage: project raw observations into typed axes that
//! decide consumes. One module per axis, building bottom-up.
//! The shared shape (Facts / Findings / Opportunities, etc.) emerges
//! once a second axis lands and forces the abstraction.

pub mod bot_threads;
pub mod ci;
pub mod copilot;
pub mod cursor;
pub mod pr_meta;
pub mod required_checks;
pub mod reviews;
pub mod state;
pub mod thread;

use crate::ids::Timestamp;
use crate::observe::github::GitHubObservations;
use crate::observe::github::compare::MergeBaseDelta;
use serde::Serialize;

use ci::CiReport;
use copilot::{CopilotRepoConfig, CopilotReport, orient_copilot};
use cursor::{CursorReport, orient_cursor};
use pr_meta::{PrMetadata, orient_pr_meta};
use reviews::ReviewSummary;
use state::PullRequestState;
use thread::ReviewThread;

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
#[derive(Debug, Clone, Serialize)]
pub struct OrientedState {
    pub ci: CiReport,
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
    /// All review threads on the PR, projected from the wire model
    /// into the typed domain. Always-present (empty vec ≡ no
    /// threads); each carries author, location, body, and lifecycle
    /// state. The witness for `AddressThreads` actions and the
    /// canonical source for any per-author thread query.
    pub threads: Vec<ReviewThread>,
    /// Merge-base delta surfaced as-is from observe — pure
    /// pass-through with no axis-specific projection. `None` when
    /// the compare endpoint was unavailable (terminal PR, base ref
    /// race). Consumed by decide's Rebase emission to surface the
    /// concrete file overlap rather than a generic "rebase now."
    pub merge_base_delta: Option<MergeBaseDelta>,
    /// PR-meta sync state. Projects the observe-side attestation
    /// file + HEAD-SHA comparison into `Synced` / `Drift` /
    /// `NeverAttested`. Drives the `SyncPrMeta` decide candidate.
    pub pr_metadata: PrMetadata,
    /// Absolute path of the attestation file the agent must rewrite
    /// after refreshing PR metadata. Carried from observe so decide
    /// can compose the `SyncPrMeta` action payload without re-deriving
    /// the path. `None` when no `--state-root` was supplied to the
    /// binary.
    pub attest_path: Option<std::path::PathBuf>,
}

/// Compose all axes from a single GitHub observation bundle.
///
/// `last_commit_at` comes from outside the GitHub fetch bundle
/// (typically `git log` on HEAD); pass `None` if unavailable.
/// `now` is the wall-clock at the start of this orient pass — read
/// once by the runner per iteration and threaded through axes that
/// need a clock (currently: copilot health). Tests pass fixed
/// values to keep behavior deterministic.
pub fn orient(
    obs: &GitHubObservations,
    last_commit_at: Option<Timestamp>,
    now: Timestamp,
) -> OrientedState {
    let required =
        required_checks::required_check_names(&obs.branch_rules, obs.branch_protection.as_ref());
    let pr_state = state::orient_state(&obs.pr_view, last_commit_at, &obs.stack_root_branch);
    // The Graphite mergeability check pends indefinitely on a PR
    // stacked under an open parent; treat it as advisory rather than
    // a required wait for those PRs so the loop halts `Paused` once
    // other work clears instead of cycling `WaitForCi` to the cap.
    let ci = ci::orient_ci(
        &obs.checks,
        &required,
        pr_state.has_open_parent_pr,
        &obs.workflow_runs,
        &obs.pr_view.head_ref_oid,
        now,
    );
    let reviews = reviews::orient_reviews(
        &obs.pr_view,
        &obs.review_threads_page,
        &obs.issue_comments,
        &obs.reviews,
        &obs.requested_reviewers,
    );
    let copilot = obs
        .copilot_config
        .map(CopilotRepoConfig::from)
        .and_then(|cfg| {
            orient_copilot(
                cfg,
                &obs.issue_events,
                &obs.reviews,
                &obs.review_threads_page,
                &obs.requested_reviewers,
                &obs.pr_view.head_ref_oid,
                &obs.pr_view.commits,
                now,
            )
        });
    let cursor = orient_cursor(
        &obs.reviews,
        &obs.review_threads_page,
        &obs.cursor_status,
        obs.pr_view.author.as_ref(),
        &obs.pr_view.head_ref_oid,
        now,
    );

    let threads: Vec<ReviewThread> = obs
        .review_threads_page
        .data
        .repository
        .pull_request
        .review_threads
        .nodes
        .iter()
        .filter_map(ReviewThread::from_wire)
        .collect();

    let pr_metadata = orient_pr_meta(&obs.pr_meta);
    let attest_path = obs.pr_meta.attest_path.clone();

    OrientedState {
        ci,
        state: pr_state,
        reviews,
        copilot,
        cursor,
        threads,
        merge_base_delta: obs.merge_base_delta.clone(),
        pr_metadata,
        attest_path,
    }
}
