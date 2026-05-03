//! Resolve the active Copilot ruleset config for a repo.
//!
//! Walks the ruleset list, fetches each detail in parallel, returns
//! the first active `copilot_code_review` rule's parameters. Most
//! repos have 3-10 rulesets; sequential takes ~500ms × N, parallel
//! takes ~500ms total.

use std::thread;

use crate::ids::RepoSlug;

use super::gh::GhError;
use super::rulesets::{
    fetch_ruleset, fetch_ruleset_list, ruleset_matches_branch,
    CopilotCodeReviewParams, RulesetEnforcement,
};

/// Returns:
///   * `Ok(Some(params))` — at least one active ruleset has a
///     `copilot_code_review` rule AND the ruleset's branch-scope
///     conditions cover `branch`.
///   * `Ok(None)` — no qualifying ruleset (none active with the
///     rule, or the rulesets that have the rule don't apply to
///     this branch).
///   * `Err(_)` — the list call failed, or a non-404 error on a
///     detail fetch. Per-detail 404s are skipped (a ruleset can
///     vanish between list and detail).
///
/// `branch` is the resolved stack-root branch. Pre-fix, the FIRST
/// active ruleset with `copilot_code_review` won — even when its
/// conditions excluded the PR's branch — producing misleading
/// "Copilot configured" status and pointless rerequest waits.
pub fn fetch_copilot_config(
    slug: &RepoSlug,
    branch: &str,
) -> Result<Option<CopilotCodeReviewParams>, GhError> {
    let summaries = fetch_ruleset_list(slug)?;
    if summaries.is_empty() {
        return Ok(None);
    }

    thread::scope(|s| {
        let handles: Vec<_> = summaries
            .into_iter()
            .map(|summary| s.spawn(move || extract_copilot(slug, summary.id, branch)))
            .collect();

        for h in handles {
            match h.join().expect("fetch_ruleset panicked")? {
                Some(params) => return Ok(Some(params)),
                None => continue,
            }
        }
        Ok(None)
    })
}

fn extract_copilot(
    slug: &RepoSlug,
    id: u64,
    branch: &str,
) -> Result<Option<CopilotCodeReviewParams>, GhError> {
    let ruleset = match fetch_ruleset(slug, id) {
        Ok(r) => r,
        Err(GhError::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    if ruleset.enforcement != RulesetEnforcement::Active {
        return Ok(None);
    }
    if !ruleset_matches_branch(ruleset.conditions.as_ref(), branch) {
        return Ok(None);
    }
    for rule in ruleset.rules {
        if rule.rule_type != "copilot_code_review" {
            continue;
        }
        // Missing `parameters` is a valid GitHub shape — it means
        // the ruleset uses defaults (review_on_push=false,
        // review_draft_pull_requests=false). Pre-fix this branch
        // skipped the rule and fetch_copilot_config returned None,
        // misreporting Copilot as not-configured for any repo
        // using default settings; PRs would never wait for or
        // re-request the required Copilot review.
        let parsed = match rule.parameters {
            Some(p) => match serde_json::from_value::<CopilotCodeReviewParams>(p) {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => CopilotCodeReviewParams {
                review_on_push: false,
                review_draft_pull_requests: false,
            },
        };
        return Ok(Some(parsed));
    }
    Ok(None)
}
