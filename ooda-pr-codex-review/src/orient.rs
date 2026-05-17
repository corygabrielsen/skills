//! Orient stage: project raw observations into per-axis typed
//! reports.
//!
//! Boundary: input is the observation bundle from upstream; output
//! is one report per axis. Axes are independent — each owns its
//! own classifier — and the surface is deliberately heterogeneous
//! (a `None` axis is structurally distinct from an axis whose state
//! reads "idle"). Decision logic does not live here.

pub(crate) mod bot_threads;
pub(crate) mod ci;
pub(crate) mod claude_review;
pub(crate) mod closeout;
pub(crate) mod codex_review;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod doc_review;
pub(crate) mod pull_request_metadata;
pub(crate) mod required_checks;
pub(crate) mod reviews;
pub(crate) mod state;
pub(crate) mod thread;

use crate::ids::Timestamp;
use crate::observe::codex::CodexObservations;
use crate::observe::github::GitHubObservations;
use crate::observe::github::compare::MergeBaseDelta;
use serde::Serialize;

use ci::CiReport;
use claude_review::{ClaudeReview, orient_claude_review};
use closeout::{Closeout, orient_closeout};
pub(crate) use codex_review::CodexReviewReport;
use codex_review::orient_codex_review;
use copilot::{CopilotRepoConfig, CopilotReport, orient_copilot};
use cursor::{CursorReport, orient_cursor};
use doc_review::{DocReview, orient_doc_review};
use pull_request_metadata::{PullRequestMetadata, orient_pull_request_metadata};
use reviews::ReviewSummary;
use state::PullRequestProjection;
use thread::ReviewThread;

/// Transient post-orient bundle carrying every axis's projection.
///
/// **Not a stable consumer-facing contract.** Each downstream
/// consumer takes a per-consumer `<X>Inputs<'a>` struct of typed
/// dep refs; the `From<&OrientedState>` bridges defined on each
/// `Inputs` struct decompose this bundle. No consumer body reads
/// `OrientedState` fields directly — the Driver
/// ([`crate::runner::drive`]) dispatches per-axis via the
/// [`ooda_core::Axis`] trait, and each `Inputs` constructor
/// projects out the subset of fields its consumer needs.
///
/// Composition is purely additive: each axis owns its own field, no
/// cross-axis aggregate (score, tier) lives here. Downstream views
/// derive aggregates on demand.
///
/// Optionality is asymmetric by intent. An always-present axis
/// (`ci`, `state`, `reviews`) admits a normal empty/idle state in
/// its own variant. An `Option` axis (`copilot`, `cursor`,
/// `codex_review`) reserves `None` to encode *no axis applicable
/// here* — distinct from *axis applies but is currently quiet*.
/// Collapsing those two onto a single quiet-state would readmit
/// the false-halt bug the optionality is here to forbid.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OrientedState {
    pub ci: CiReport,
    pub state: PullRequestProjection,
    pub reviews: ReviewSummary,
    /// `None` when no configuration surface enables this axis on
    /// the repo. `Some(report)` with an idle activity is the
    /// distinct "enabled but quiet on this PR" state.
    pub copilot: Option<CopilotReport>,
    /// `None` when no activity surface evidences this axis on the
    /// PR. The axis exposes no separate configuration surface, so
    /// applicability is detectable only through activity.
    pub cursor: Option<CursorReport>,
    /// Review threads projected from wire model to typed domain.
    /// Empty vec encodes "no threads"; each entry carries author,
    /// location, body, and lifecycle state.
    pub threads: Vec<ReviewThread>,
    /// `None` when the axis is not configured at this invocation.
    /// `Some(report)` carries the ladder slice's current status
    /// plus the side-effect surface (directory, head SHA) the
    /// spawn and scan layers need.
    pub codex_review: Option<CodexReviewReport>,
    /// Merge-base delta surfaced as-is. `None` when the upstream
    /// compare endpoint was unavailable. Consumed where the
    /// concrete file overlap improves a generic rebase prompt.
    pub merge_base_delta: Option<MergeBaseDelta>,
    /// SHA-keyed attestation: PR metadata is `Synced` /
    /// `Drift` / `NeverAttested` relative to HEAD.
    pub pull_request_metadata: PullRequestMetadata,
    /// Attestation-file path co-carried with the corresponding
    /// axis state so a downstream action payload need not re-derive
    /// it. `None` when no state root was configured.
    pub attest_path: Option<std::path::PathBuf>,
    /// SHA-keyed attestation: an independent claim that an agent
    /// has reviewed the PR diff for doc and comment hygiene.
    pub doc_review: DocReview,
    /// Attestation-file path for the doc-review axis.
    pub doc_review_attest_path: Option<std::path::PathBuf>,
    /// Content-keyed attestation. Unlike the SHA-keyed axes, drift
    /// is computed against content timestamps rather than HEAD —
    /// the underlying surface changes without a new commit.
    pub claude_review: ClaudeReview,
    /// Attestation-file path for the claude-review axis.
    pub claude_review_attest_path: Option<std::path::PathBuf>,
    /// Convergence-gate attestation. Same SHA-keyed shape as the
    /// other attestation axes; sits at the least-urgent tier so it
    /// fires only after every other axis is quiet, making the
    /// terminal handoff conditional on an agent's pre-handoff
    /// sign-off at HEAD.
    pub closeout: Closeout,
    /// Attestation-file path for the closeout axis.
    pub closeout_attest_path: Option<std::path::PathBuf>,
}

/// Compose all axes from one observation bundle plus optional
/// codex-review observations.
///
/// `now` is the wall-clock for the iteration — read once upstream
/// and threaded through axes that need a clock, so every axis sees
/// the same instant and tests can pin determinism. `last_commit_at`
/// is sourced outside the upstream fetch bundle; `None` is the
/// well-defined unavailable case. `codex_obs` is `Some` exactly
/// when the codex-review axis is enabled at this invocation; `None`
/// makes the axis structurally absent and is observationally
/// equivalent to the non-codex configuration.
pub(crate) fn orient(
    obs: &GitHubObservations,
    codex_obs: Option<&CodexObservations>,
    last_commit_at: Option<Timestamp>,
    now: Timestamp,
) -> OrientedState {
    let required =
        required_checks::required_check_names(&obs.branch_rules, obs.branch_protection.as_ref());
    let pr_state = state::orient_state(
        &obs.pull_request_view,
        last_commit_at,
        &obs.stack_root_branch,
        &obs.branch_rules,
        &obs.checks,
    );
    // When a PR is stacked under an unmerged parent, a mergeability
    // check pends indefinitely by design. Demoting it to advisory
    // for those PRs prevents the loop from cycling Wait to the cap
    // on a gate that cannot resolve at this layer.
    let ci = ci::orient_ci(
        &obs.checks,
        &required,
        pr_state.has_open_parent_pr,
        &obs.workflow_runs,
        &obs.pull_request_view.head_ref_oid,
        now,
    );
    let reviews = reviews::orient_reviews(
        &obs.pull_request_view,
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
                &obs.pull_request_view.head_ref_oid,
                &obs.pull_request_view.commits,
                now,
            )
        });
    let cursor = orient_cursor(
        &obs.reviews,
        &obs.review_threads_page,
        &obs.cursor_status,
        obs.pull_request_view.author.as_ref(),
        &obs.pull_request_view.head_ref_oid,
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

    let codex_review = codex_obs.map(orient_codex_review);

    let pull_request_metadata = orient_pull_request_metadata(&obs.pull_request_metadata);
    let attest_path = obs.pull_request_metadata.attest_path.clone();
    let doc_review = orient_doc_review(&obs.doc_review);
    let doc_review_attest_path = obs.doc_review.attest_path.clone();
    let claude_review = orient_claude_review(&obs.claude_review);
    let claude_review_attest_path = obs.claude_review.attest_path.clone();
    let closeout = orient_closeout(&obs.closeout);
    let closeout_attest_path = obs.closeout.attest_path.clone();

    OrientedState {
        ci,
        state: pr_state,
        reviews,
        copilot,
        cursor,
        threads,
        codex_review,
        merge_base_delta: obs.merge_base_delta.clone(),
        pull_request_metadata,
        attest_path,
        doc_review,
        doc_review_attest_path,
        claude_review,
        claude_review_attest_path,
        closeout,
        closeout_attest_path,
    }
}
