//! GitHub-sourced observations (REST + GraphQL via `gh` CLI).

pub mod branch_protection;
pub mod branch_rules;
pub mod checks;
pub mod comments;
pub mod compare;
pub mod copilot_config;
pub mod cursor_status;
pub mod doc_review_attest;
pub mod gh;
pub mod issue_events;
pub mod pull_request_metadata_attestation;
pub mod pull_request_view;
pub mod rate_limit;
pub mod requested_reviewers;
pub mod review_threads;
pub mod reviews;
pub mod rulesets;
pub mod stack_root;
pub mod workflow_runs;

use std::thread;

use crate::ids::{PullRequestNumber, RepoSlug};
use ooda_core::{RateLimitBudget, RateLimitHit};
use serde::Serialize;

use branch_protection::{
    BranchProtectionRequiredStatusChecks, fetch_branch_protection_required_checks,
};
use branch_rules::{BranchRule, fetch_branch_rules};
use checks::{PullRequestCheck, fetch_pull_request_checks};
use comments::{IssueComment, fetch_issue_comments};
use compare::{MergeBaseDelta, fetch_merge_base_delta};
use copilot_config::fetch_copilot_config;
use cursor_status::{CursorStatus, fetch_cursor_status};
use doc_review_attest::{DocReviewObservation, observe_doc_review};
use gh::GhError;
use issue_events::{IssueEvent, fetch_issue_events};
use pull_request_metadata_attestation::{
    PullRequestMetadataObservation, observe_pull_request_metadata,
};
use pull_request_view::{PullRequestState, PullRequestView, fetch_pull_request_view};
use rate_limit::fetch_rate_limit_budget;
use requested_reviewers::{RequestedReviewers, fetch_requested_reviewers};
use review_threads::{
    ReviewThreadsResponse, empty_review_threads_response, fetch_all_review_threads,
};
use reviews::{PullRequestReview, fetch_pull_request_reviews};
use rulesets::CopilotCodeReviewParams;
use stack_root::resolve_stack_root;
use workflow_runs::{WorkflowRun, fetch_workflow_runs_for_head};

/// Successful outcome of [`fetch_all`]. Either a full observation
/// bundle (the loop proceeds to orient/decide) or a [`RateLimitHit`]
/// (the runner emits a `WaitForRateLimit` and re-observes after the
/// scope's retry window). [`GhError`] is reserved for non-recoverable
/// failures: spawn errors, parse errors, real non-2xx responses.
#[derive(Debug, Clone, Serialize)]
pub enum FetchOutcome {
    Observations(Box<GitHubObservations>),
    RateLimited(RateLimitHit),
}

/// Full PR-scoped observation bundle from GitHub. Produced by
/// [`fetch_all`]; consumed by the orient stage.
///
/// `review_threads_page` holds *all* threads — `fetch_all_review_threads`
/// loops the GraphQL cursor until the last page.
#[derive(Debug, Clone, Serialize)]
pub struct GitHubObservations {
    pub pull_request_view: PullRequestView,
    pub checks: Vec<PullRequestCheck>,
    pub reviews: Vec<PullRequestReview>,
    pub review_threads_page: ReviewThreadsResponse,
    pub issue_events: Vec<IssueEvent>,
    pub issue_comments: Vec<IssueComment>,
    pub requested_reviewers: RequestedReviewers,
    pub branch_rules: Vec<BranchRule>,
    /// `None` when legacy branch protection is unconfigured (HTTP 404).
    pub branch_protection: Option<BranchProtectionRequiredStatusChecks>,
    /// Branch the rules + protection were resolved against. Differs
    /// from `pull_request_view.base_ref_name` when this PR is mid-stack and
    /// the protected branch is downstream.
    pub stack_root_branch: crate::ids::BranchName,
    /// `None` when no active ruleset has a `copilot_code_review`
    /// rule. Resolved by walking ruleset summaries + details during
    /// `fetch_all`.
    pub copilot_config: Option<CopilotCodeReviewParams>,
    /// Snapshot of remaining GitHub quota across the buckets the
    /// loop uses. Fetched via the free `/rate_limit` endpoint; today
    /// nothing acts on this beyond the recorder's per-iteration log.
    /// See [`ooda_core::RateLimitBudget`] for the named-but-unimplemented
    /// routing concepts.
    pub rate_limit_budget: RateLimitBudget,
    /// All workflow runs on the current HEAD SHA. Source of per-check
    /// `created_at` / `run_started_at` (the CI health detector's
    /// queue/run timeouts) and per-(name, HEAD) attempt counts (the
    /// re-run budget). Bounded N — a single HEAD typically has 0-30
    /// runs.
    pub workflow_runs: Vec<WorkflowRun>,
    /// Cursor's `check_suite` + `check_run` on the current HEAD. Distinct
    /// from `checks` (which aggregates by check name and drops
    /// suite-level state) and from `workflow_runs` (Cursor is a
    /// third-party app, not a GHA workflow). Source of the
    /// stuck-suite stall signal — `gh pr checks` can't see a
    /// `check_suite` that never spawned a child `check_run`.
    pub cursor_status: CursorStatus,
    /// Merge-base delta between the PR head and its immediate base —
    /// commits behind, files master touched since base, and the
    /// intersection with branch-touched files. `None` when the
    /// compare endpoint failed for this PR (e.g. base ref deleted
    /// post-merge in a race); decide treats absence as "no extra
    /// detail," not as an error.
    pub merge_base_delta: Option<MergeBaseDelta>,
    /// PR-meta attestation snapshot: the recorded attestation (if
    /// any), the current HEAD SHA, and the count of commits the PR
    /// has added since the attestation. Drives the `PullRequestMetadata`
    /// orient axis. Absent attestation collapses to
    /// `NeverAttested`; a `Some(0)` count after a drift compare
    /// failure still classifies as Drift (orient inspects both
    /// SHAs).
    pub pull_request_metadata: PullRequestMetadataObservation,
    /// Doc-review attestation snapshot. Same shape as
    /// `pull_request_metadata`; drives the `DocReview` orient axis.
    pub doc_review: DocReviewObservation,
}

/// Fetch every GitHub observation needed to describe the PR's state.
///
/// Three phases:
///   1. `fetch_pull_request_view` — needed both for terminal short-circuit
///      and for `base_ref_name` used by branch-level endpoints.
///   2. Terminal short-circuit — Merged/Closed PRs skip the
///      auxiliary fetches entirely. Branch rules + protection
///      hit `rules/branches/{base}` which 404s when the base
///      branch was deleted post-merge; without this short-circuit,
///      every merged-PR observation fails as a transport error
///      instead of `decide()`'s documented `Halt::Terminal`.
///   3. Parallel aux fetch — the remaining nine calls fan out
///      concurrently. Fail-fast on the first error.
pub fn fetch_all(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    state_root: Option<&std::path::Path>,
) -> Result<FetchOutcome, GhError> {
    /// Promote `GhError::RateLimited` to early-return
    /// `Ok(FetchOutcome::RateLimited)`. Real errors propagate.
    macro_rules! try_fetch {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(GhError::RateLimited(hit)) => {
                    return Ok(FetchOutcome::RateLimited(hit));
                }
                Err(e) => return Err(e),
            }
        };
    }

    let pull_request_view = try_fetch!(fetch_pull_request_view(slug, pr));
    if matches!(pull_request_view.state, PullRequestState::Terminal(_)) {
        // Terminal PRs still get a budget snapshot — the recorder
        // sees one final per-iteration row before the loop halts.
        let rate_limit_budget = try_fetch!(fetch_rate_limit_budget());
        return Ok(FetchOutcome::Observations(Box::new(terminal_observations(
            pull_request_view,
            rate_limit_budget,
        ))));
    }
    let pull_request_metadata =
        observe_pull_request_metadata(state_root, slug, pr, &pull_request_view.head_ref_oid);
    let doc_review = observe_doc_review(state_root, slug, pr, &pull_request_view.head_ref_oid);
    // Branch rules and protection live at the protected root, not
    // at intermediate stack branches. Resolve before fanning out.
    let stack_root_branch = try_fetch!(resolve_stack_root(slug, &pull_request_view.base_ref_name));
    let root_for_threads = stack_root_branch.clone();
    let head_sha = pull_request_view.head_ref_oid.clone();

    thread::scope(|s| {
        let root = root_for_threads.as_str();
        let h_checks = s.spawn(|| fetch_pull_request_checks(slug, pr));
        let h_reviews = s.spawn(|| fetch_pull_request_reviews(slug, pr));
        let h_threads = s.spawn(|| fetch_all_review_threads(slug, pr));
        let h_events = s.spawn(|| fetch_issue_events(slug, pr));
        let h_comments = s.spawn(|| fetch_issue_comments(slug, pr));
        let h_reqrev = s.spawn(|| fetch_requested_reviewers(slug, pr));
        let h_rules = s.spawn(move || fetch_branch_rules(slug, root));
        let h_prot = s.spawn(move || fetch_branch_protection_required_checks(slug, root));
        let h_copilot_cfg = s.spawn(move || fetch_copilot_config(slug, root));
        // `/rate_limit` does not count against quota; fan it in
        // alongside the others so the snapshot is roughly
        // coincident with the rest of the observation bundle.
        let h_rate_limit = s.spawn(fetch_rate_limit_budget);
        let h_workflow_runs = {
            let head_for_runs = head_sha.clone();
            s.spawn(move || fetch_workflow_runs_for_head(slug, &head_for_runs))
        };
        let h_cursor_status = {
            let head_for_cursor = head_sha.clone();
            s.spawn(move || fetch_cursor_status(slug, &head_for_cursor))
        };
        let h_compare = {
            // Keyed on the PR's immediate base (not the resolved
            // stack root) so a stacked PR's compare describes the
            // local rebase target, not the trunk far below.
            let base_for_compare = pull_request_view.base_ref_name.clone();
            let head_for_compare = head_sha.clone();
            s.spawn(move || fetch_merge_base_delta(slug, &base_for_compare, &head_for_compare))
        };

        Ok(FetchOutcome::Observations(Box::new(GitHubObservations {
            pull_request_view,
            checks: try_fetch!(h_checks.join().expect("fetch_pull_request_checks panicked")),
            reviews: try_fetch!(
                h_reviews
                    .join()
                    .expect("fetch_pull_request_reviews panicked")
            ),
            review_threads_page: try_fetch!(h_threads.join().expect("fetch_threads panicked")),
            issue_events: try_fetch!(h_events.join().expect("fetch_issue_events panicked")),
            issue_comments: try_fetch!(h_comments.join().expect("fetch_issue_comments panicked")),
            requested_reviewers: try_fetch!(h_reqrev.join().expect("fetch_reqrev panicked")),
            branch_rules: try_fetch!(h_rules.join().expect("fetch_branch_rules panicked")),
            branch_protection: try_fetch!(h_prot.join().expect("fetch_branch_protection panicked")),
            copilot_config: try_fetch!(
                h_copilot_cfg.join().expect("fetch_copilot_config panicked")
            ),
            stack_root_branch,
            rate_limit_budget: try_fetch!(h_rate_limit.join().expect("fetch_rate_limit panicked")),
            workflow_runs: try_fetch!(
                h_workflow_runs
                    .join()
                    .expect("fetch_workflow_runs panicked")
            ),
            cursor_status: try_fetch!(
                h_cursor_status
                    .join()
                    .expect("fetch_cursor_status panicked")
            ),
            // Compare endpoint 404s after a base-ref delete race —
            // tolerate that case the same way `branch_protection`
            // does (collapse to None), surface every other failure.
            merge_base_delta: match h_compare.join().expect("fetch_merge_base_delta panicked") {
                Ok(delta) => Some(delta),
                Err(GhError::NotFound) => None,
                Err(GhError::RateLimited(hit)) => {
                    return Ok(FetchOutcome::RateLimited(hit));
                }
                Err(e) => return Err(e),
            },
            pull_request_metadata,
            doc_review,
        })))
    })
}

/// Stub bundle for terminal (merged/closed) PRs. `decide()` will
/// short-circuit on `pull_request_view.state` before reading any of the
/// empty aux fields, so semantic correctness is preserved while
/// avoiding the deleted-base-branch 404 that the auxiliary
/// fetches would otherwise hit.
fn terminal_observations(
    pull_request_view: PullRequestView,
    rate_limit_budget: RateLimitBudget,
) -> GitHubObservations {
    let stack_root_branch = pull_request_view.base_ref_name.clone();
    let head_sha = pull_request_view.head_ref_oid.clone();
    GitHubObservations {
        pull_request_view,
        checks: vec![],
        reviews: vec![],
        review_threads_page: empty_review_threads_response(),
        issue_events: vec![],
        issue_comments: vec![],
        requested_reviewers: RequestedReviewers::default(),
        branch_rules: vec![],
        branch_protection: None,
        stack_root_branch,
        copilot_config: None,
        rate_limit_budget,
        workflow_runs: vec![],
        cursor_status: CursorStatus {
            suite: None,
            run: None,
        },
        // Terminal PRs (merged / closed) have no live merge base to
        // describe — the compare fetch is skipped along with the
        // rest of the parallel aux pass.
        merge_base_delta: None,
        // Terminal PRs short-circuit before reading state-root.
        // Stub the attestation observation so the orient layer can
        // walk uniformly; decide short-circuits on pull_request_view.state
        // before reading any field of pull_request_metadata.
        pull_request_metadata: PullRequestMetadataObservation {
            attestation: None,
            head_sha: head_sha.clone(),
            commits_behind: None,
            attest_path: None,
        },
        doc_review: DocReviewObservation {
            attestation: None,
            head_sha,
            commits_behind: None,
            attest_path: None,
        },
    }
}
