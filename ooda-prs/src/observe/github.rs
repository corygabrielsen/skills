//! GitHub-sourced observations (REST + GraphQL via `gh` CLI).

pub mod branch_protection;
pub mod branch_rules;
pub mod checks;
pub mod comments;
pub mod copilot_config;
pub mod gh;
pub mod issue_events;
pub mod pr_view;
pub mod requested_reviewers;
pub mod review_threads;
pub mod reviews;
pub mod rulesets;
pub mod stack_root;

use std::thread;

use crate::ids::{PullRequestNumber, RepoSlug};
use ooda_core::RateLimitHit;
use serde::Serialize;

use branch_protection::{
    BranchProtectionRequiredStatusChecks, fetch_branch_protection_required_checks,
};
use branch_rules::{BranchRule, fetch_branch_rules};
use checks::{PullRequestCheck, fetch_pr_checks};
use comments::{IssueComment, fetch_issue_comments};
use copilot_config::fetch_copilot_config;
use gh::GhError;
use issue_events::{IssueEvent, fetch_issue_events};
use pr_view::{PrState, PullRequestView, fetch_pr_view};
use requested_reviewers::{RequestedReviewers, fetch_requested_reviewers};
use review_threads::{
    ReviewThreadsResponse, empty_review_threads_response, fetch_all_review_threads,
};
use reviews::{PullRequestReview, fetch_pr_reviews};
use rulesets::CopilotCodeReviewParams;
use stack_root::resolve_stack_root;

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
    pub pr_view: PullRequestView,
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
    /// from `pr_view.base_ref_name` when this PR is mid-stack and
    /// the protected branch is downstream.
    pub stack_root_branch: crate::ids::BranchName,
    /// `None` when no active ruleset has a `copilot_code_review`
    /// rule. Resolved by walking ruleset summaries + details during
    /// fetch_all.
    pub copilot_config: Option<CopilotCodeReviewParams>,
}

/// Fetch every GitHub observation needed to describe the PR's state.
///
/// Three phases:
///   1. `fetch_pr_view` — needed both for terminal short-circuit
///      and for `base_ref_name` used by branch-level endpoints.
///   2. Terminal short-circuit — Merged/Closed PRs skip the
///      auxiliary fetches entirely. Branch rules + protection
///      hit `rules/branches/{base}` which 404s when the base
///      branch was deleted post-merge; without this short-circuit,
///      every merged-PR observation fails as a transport error
///      instead of decide()'s documented `Halt::Terminal`.
///   3. Parallel aux fetch — the remaining nine calls fan out
///      concurrently. Fail-fast on the first error.
pub fn fetch_all(slug: &RepoSlug, pr: PullRequestNumber) -> Result<FetchOutcome, GhError> {
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

    let pr_view = try_fetch!(fetch_pr_view(slug, pr));
    if matches!(pr_view.state, PrState::Terminal(_)) {
        return Ok(FetchOutcome::Observations(Box::new(terminal_observations(
            pr_view,
        ))));
    }
    // Branch rules and protection live at the protected root, not
    // at intermediate stack branches. Resolve before fanning out.
    let stack_root_branch = try_fetch!(resolve_stack_root(slug, &pr_view.base_ref_name));
    let root_for_threads = stack_root_branch.clone();

    thread::scope(|s| {
        let root = root_for_threads.as_str();
        let h_checks = s.spawn(|| fetch_pr_checks(slug, pr));
        let h_reviews = s.spawn(|| fetch_pr_reviews(slug, pr));
        let h_threads = s.spawn(|| fetch_all_review_threads(slug, pr));
        let h_events = s.spawn(|| fetch_issue_events(slug, pr));
        let h_comments = s.spawn(|| fetch_issue_comments(slug, pr));
        let h_reqrev = s.spawn(|| fetch_requested_reviewers(slug, pr));
        let h_rules = s.spawn(move || fetch_branch_rules(slug, root));
        let h_prot = s.spawn(move || fetch_branch_protection_required_checks(slug, root));
        let h_copilot_cfg = s.spawn(move || fetch_copilot_config(slug, root));

        Ok(FetchOutcome::Observations(Box::new(GitHubObservations {
            pr_view,
            checks: try_fetch!(h_checks.join().expect("fetch_pr_checks panicked")),
            reviews: try_fetch!(h_reviews.join().expect("fetch_pr_reviews panicked")),
            review_threads_page: try_fetch!(
                h_threads
                    .join()
                    .expect("fetch_review_threads_page panicked")
            ),
            issue_events: try_fetch!(h_events.join().expect("fetch_issue_events panicked")),
            issue_comments: try_fetch!(h_comments.join().expect("fetch_issue_comments panicked")),
            requested_reviewers: try_fetch!(
                h_reqrev.join().expect("fetch_requested_reviewers panicked")
            ),
            branch_rules: try_fetch!(h_rules.join().expect("fetch_branch_rules panicked")),
            branch_protection: try_fetch!(
                h_prot
                    .join()
                    .expect("fetch_branch_protection_required_checks panicked")
            ),
            copilot_config: try_fetch!(
                h_copilot_cfg.join().expect("fetch_copilot_config panicked")
            ),
            stack_root_branch,
        })))
    })
}

/// Stub bundle for terminal (merged/closed) PRs. decide() will
/// short-circuit on `pr_view.state` before reading any of the
/// empty aux fields, so semantic correctness is preserved while
/// avoiding the deleted-base-branch 404 that the auxiliary
/// fetches would otherwise hit.
fn terminal_observations(pr_view: PullRequestView) -> GitHubObservations {
    let stack_root_branch = pr_view.base_ref_name.clone();
    GitHubObservations {
        pr_view,
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
    }
}
