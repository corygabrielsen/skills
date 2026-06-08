//! Host-sourced observations.
//!
//! # Invariants
//!
//! - **One observation bundle per iteration**: every observe pass
//!   yields a single immutable bundle; downstream stages never
//!   re-fetch.
//! - **Fail-fast on real errors, surface rate-limit as data**:
//!   transport / parse failures abort the pass; rate-limit hits are
//!   typed and surfaced so decide emits a wait action instead of
//!   crashing the loop.
//! - **Terminal short-circuit**: terminal PRs skip auxiliary fetches
//!   that depend on a live base ref. Without this, post-merge base
//!   deletion races would fail the whole pass on otherwise-done PRs.

pub(crate) mod branch_protection;
pub(crate) mod branch_rules;
pub(crate) mod checks;
pub(crate) mod claude_review_attest;
pub(crate) mod closeout_attest;
pub(crate) mod comments;
pub(crate) mod compare;
pub(crate) mod copilot_config;
pub(crate) mod cursor_status;
pub(crate) mod doc_review_attest;
pub(crate) mod gh;
pub(crate) mod issue_events;
pub(crate) mod pull_request_metadata_attestation;
pub(crate) mod pull_request_view;
pub(crate) mod rate_limit;
pub(crate) mod requested_reviewers;
pub(crate) mod review_threads;
pub(crate) mod reviews;
pub(crate) mod rulesets;
pub(crate) mod stack_root;
pub(crate) mod workflow_runs;

use std::thread;

use crate::ids::{PullRequestNumber, RepoSlug};
use ooda_core::{RateLimitBudget, RateLimitHit};
use serde::Serialize;

use branch_protection::{BranchProtection, fetch_branch_protection};
use branch_rules::{BranchRule, fetch_branch_rules};
use checks::{PullRequestCheck, fetch_pull_request_checks};
use claude_review_attest::{ClaudeReviewObservation, observe_claude_review};
use closeout_attest::{CloseoutObservation, observe_closeout};
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

/// Successful observe outcome. A rate-limit hit is data (decide emits
/// a wait); transport, spawn, and parse failures are errors that
/// abort the pass.
#[derive(Debug, Clone, Serialize)]
pub(crate) enum FetchOutcome {
    Observations(Box<GitHubObservations>),
    RateLimited(RateLimitHit),
}

pub(crate) use super::branch::BranchSyncObservation;

/// Per-PR host-side observation bundle. The field named
/// `review_threads_page` holds every thread on the PR — the cursor
/// loop has already been drained before the bundle is assembled.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GitHubObservations {
    pub pull_request_view: PullRequestView,
    pub checks: Vec<PullRequestCheck>,
    pub reviews: Vec<PullRequestReview>,
    pub review_threads_page: ReviewThreadsResponse,
    pub issue_events: Vec<IssueEvent>,
    pub issue_comments: Vec<IssueComment>,
    pub requested_reviewers: RequestedReviewers,
    pub branch_rules: Vec<BranchRule>,
    /// Absent when the legacy-protection source is unconfigured.
    pub branch_protection: Option<BranchProtection>,
    /// Branch the rule sources were resolved against. Diverges from
    /// the PR's immediate base when the PR is mid-stack and the
    /// protected branch sits downstream.
    pub stack_root_branch: crate::ids::BranchName,
    /// Absent when no active rule source declares a reviewer-axis
    /// rule applicable to this branch.
    pub copilot_config: Option<CopilotCodeReviewParams>,
    /// Per-iteration quota snapshot. Fetched on a non-quota-
    /// consuming endpoint so the snapshot is free and coincident
    /// with the rest of the bundle.
    pub rate_limit_budget: RateLimitBudget,
    /// Run rows on the current HEAD. Source of per-check timing
    /// anchors and per-(name, HEAD) attempt counts. Bounded — a
    /// single HEAD typically carries 0-30 rows.
    pub workflow_runs: Vec<WorkflowRun>,
    /// Per-HEAD suite+run signal for the push-driven reviewer.
    /// Distinct source from `checks` (which aggregates by name and
    /// drops suite-level state) and from `workflow_runs` (the
    /// reviewer is not a workflow). Only source for the canonical
    /// stuck-suite signature.
    pub cursor_status: CursorStatus,
    /// Merge-base-relative delta between head and immediate base.
    /// Absent when the compare endpoint failed (e.g. base ref
    /// deleted in a race); decide treats absence as missing detail,
    /// not as error.
    pub merge_base_delta: Option<MergeBaseDelta>,
    /// SHA-keyed attestation snapshot — recorded attestation,
    /// current HEAD, and commits-added-since count. Drives the PR-
    /// metadata orient axis.
    pub pull_request_metadata: PullRequestMetadataObservation,
    /// SHA-keyed attestation snapshot for the doc-review axis. Same
    /// shape as the PR-metadata observation; distinct schema
    /// namespace.
    pub doc_review: DocReviewObservation,
    /// Reviewer-content attestation snapshot plus aggregated content
    /// across every reviewer-writing surface. Drives the reviewer-
    /// content orient axis.
    pub claude_review: ClaudeReviewObservation,
    /// SHA-keyed attestation snapshot for closeout. No commit-count
    /// field — HEAD-equality is the only signal the axis carries.
    pub closeout: CloseoutObservation,
    /// Branch-sync observation: divergence between the per-PR
    /// sticky head SHA and the live remote head, plus the
    /// graphite-availability probe results. Drives the
    /// `branch_sync` axis.
    pub branch_sync: BranchSyncObservation,
}

/// Fetch every observation needed to describe the PR's state.
///
/// Three phases, in order:
///   1. PR view — needed both for the terminal short-circuit and for
///      branch-level endpoint scoping.
///   2. Terminal short-circuit — terminal PRs skip auxiliaries that
///      depend on a live base. Without this, post-merge base
///      deletion races would convert a terminal-PR observation into
///      a transport error instead of a clean terminal classification.
///   3. Parallel aux fetch — remaining calls fan out concurrently;
///      fail-fast on the first non-rate-limit error.
#[allow(clippy::too_many_lines)]
pub(crate) fn fetch_all(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    state_root: Option<&std::path::Path>,
    sticky_path: Option<&std::path::Path>,
) -> Result<FetchOutcome, GhError> {
    /// Lift rate-limit hits into an early `Ok` return so they reach
    /// decide as typed data; non-rate-limit errors propagate.
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

    /// Reap a fan-out worker, converting a worker panic into a
    /// structured `GhError::NonZero` so the surrounding `try_fetch!`
    /// arm handles it on the error path rather than unwinding through
    /// `thread::scope` and aborting every sibling fetcher.
    ///
    /// `label` identifies the fetch in the resulting stderr; it must
    /// match the spawn site so an operator can map the message back
    /// to a call site without grepping payloads.
    fn join_fetch<T>(
        handle: std::thread::ScopedJoinHandle<'_, Result<T, GhError>>,
        label: &str,
    ) -> Result<T, GhError> {
        match handle.join() {
            Ok(result) => result,
            Err(payload) => {
                let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "non-string panic payload".to_string()
                };
                Err(GhError::NonZero {
                    code: None,
                    stderr: format!("{label} panicked: {msg}"),
                })
            }
        }
    }

    let pull_request_view = try_fetch!(fetch_pull_request_view(slug, pr));
    if matches!(pull_request_view.state, PullRequestState::Terminal(_)) {
        // Terminal PRs still snapshot the quota budget so the final
        // per-iteration log row is complete.
        let rate_limit_budget = try_fetch!(fetch_rate_limit_budget());
        return Ok(FetchOutcome::Observations(Box::new(terminal_observations(
            pull_request_view,
            rate_limit_budget,
        ))));
    }
    let pull_request_metadata =
        observe_pull_request_metadata(state_root, slug, pr, &pull_request_view.head_ref_oid);
    let doc_review = observe_doc_review(state_root, slug, pr, &pull_request_view.head_ref_oid);
    let closeout = observe_closeout(state_root, pr, &pull_request_view.head_ref_oid);
    // Branch-level fetches must target the protected root, not the
    // intermediate stack branch. Resolve before fan-out.
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
        let h_prot = s.spawn(move || fetch_branch_protection(slug, root));
        let h_copilot_cfg = s.spawn(move || fetch_copilot_config(slug, root));
        // Quota-free endpoint; fanning in alongside the others
        // keeps the snapshot coincident with the rest of the
        // bundle.
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
            // Keyed on the immediate base (not the resolved stack
            // root) so the compare describes the local rebase
            // target, not the trunk far below.
            let base_for_compare = pull_request_view.base_ref_name.clone();
            let head_for_compare = head_sha.clone();
            s.spawn(move || fetch_merge_base_delta(slug, &base_for_compare, &head_for_compare))
        };

        let checks = try_fetch!(join_fetch(h_checks, "fetch_pull_request_checks"));
        let reviews_v = try_fetch!(join_fetch(h_reviews, "fetch_pull_request_reviews"));
        let review_threads_page = try_fetch!(join_fetch(h_threads, "fetch_threads"));
        let issue_events = try_fetch!(join_fetch(h_events, "fetch_issue_events"));
        let issue_comments = try_fetch!(join_fetch(h_comments, "fetch_issue_comments"));
        let requested_reviewers = try_fetch!(join_fetch(h_reqrev, "fetch_reqrev"));
        let branch_rules = try_fetch!(join_fetch(h_rules, "fetch_branch_rules"));
        let branch_protection = try_fetch!(join_fetch(h_prot, "fetch_branch_protection"));
        let copilot_config = try_fetch!(join_fetch(h_copilot_cfg, "fetch_copilot_config"));
        let rate_limit_budget = try_fetch!(join_fetch(h_rate_limit, "fetch_rate_limit"));
        let workflow_runs = try_fetch!(join_fetch(h_workflow_runs, "fetch_workflow_runs"));
        let cursor_status = try_fetch!(join_fetch(h_cursor_status, "fetch_cursor_status"));
        // Tolerate post-merge base-deletion race the same way the
        // legacy-protection source does (absence, not error);
        // surface every other failure.
        let merge_base_delta = match join_fetch(h_compare, "fetch_merge_base_delta") {
            Ok(delta) => Some(delta),
            Err(GhError::NotFound) => None,
            Err(GhError::RateLimited(hit)) => {
                return Ok(FetchOutcome::RateLimited(hit));
            }
            Err(e) => return Err(e),
        };

        let claude_review = observe_claude_review(
            state_root,
            pr,
            &head_sha,
            &reviews_v,
            &issue_comments,
            &review_threads_page,
        );

        let branch_sync =
            observe_branch_sync(sticky_path, &head_sha, &pull_request_view.head_ref_name);

        Ok(FetchOutcome::Observations(Box::new(GitHubObservations {
            pull_request_view,
            checks,
            reviews: reviews_v,
            review_threads_page,
            issue_events,
            issue_comments,
            requested_reviewers,
            branch_rules,
            branch_protection,
            copilot_config,
            stack_root_branch,
            rate_limit_budget,
            workflow_runs,
            cursor_status,
            merge_base_delta,
            pull_request_metadata,
            doc_review,
            claude_review,
            closeout,
            branch_sync,
        })))
    })
}

/// Compose the branch-sync observation: probe `gt` availability,
/// probe whether the PR branch is graphite-tracked, and classify
/// the sticky-vs-current head delta.
fn observe_branch_sync(
    sticky_path: Option<&std::path::Path>,
    current_head: &crate::ids::GitCommitSha,
    branch: &crate::ids::BranchName,
) -> BranchSyncObservation {
    let gt_available = super::branch::gt_available();
    let branch_graphite_tracked = gt_available && super::branch::branch_graphite_tracked(branch);
    let divergence = sticky_path
        .and_then(super::branch::read_sticky)
        .as_ref()
        .and_then(|s| super::branch::classify_divergence(Some(s), current_head));
    BranchSyncObservation {
        divergence,
        branch_graphite_tracked,
        gt_available,
    }
}

/// Bundle for terminal PRs. Auxiliary fields are stubbed because
/// decide short-circuits on the terminal state before reading them;
/// the stubs avoid the post-merge base-deletion race that auxiliary
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
        // Terminal PRs have no live merge base; the compare fetch
        // is skipped with the rest of the aux pass.
        merge_base_delta: None,
        // Attestation observations are stubbed so the orient layer
        // can walk uniformly; decide short-circuits on terminal
        // state before reading these fields.
        pull_request_metadata: PullRequestMetadataObservation {
            attestation: None,
            head_sha: head_sha.clone(),
            commits_behind: None,
            attest_path: None,
        },
        doc_review: DocReviewObservation {
            attestation: None,
            head_sha: head_sha.clone(),
            commits_behind: None,
            attest_path: None,
        },
        claude_review: ClaudeReviewObservation {
            attestation: None,
            head_sha: head_sha.clone(),
            commits_behind: None,
            attest_path: None,
            latest_claude_at: None,
            body_at: None,
            latest_claude_body: None,
            latest_claude_url: None,
            inline_thread_count: 0,
        },
        closeout: CloseoutObservation {
            attestation: None,
            head_sha,
            attest_path: None,
        },
        // Terminal PRs skip branch-sync detection: the branch is
        // either merged or closed, so out-of-band pushes carry no
        // remediation path. Stub the field; decide short-circuits
        // on the terminal state before reading it.
        branch_sync: BranchSyncObservation {
            divergence: None,
            branch_graphite_tracked: false,
            gt_available: false,
        },
    }
}
