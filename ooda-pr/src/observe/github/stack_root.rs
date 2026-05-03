//! Walk down a PR stack to find the branch it ultimately merges into.
//!
//! `branch_rules` and `branch_protection` are configured at the
//! repo's protected base (typically `master`). For a stacked PR
//! whose `base_ref_name` is some intermediate branch, querying
//! those endpoints against the intermediate branch returns nothing
//! and the required-check list reads as empty — wrong.
//!
//! This helper resolves the *root* branch by repeatedly asking
//! "is there an open PR whose head is the current branch?" and
//! following its base. Terminates when no open PR has the current
//! branch as its head (= we've reached the protected root).
//!
//! Cycles can't happen with the open-PR head→base graph in
//! practice; we still cap iterations defensively.

use serde::Deserialize;

use crate::ids::{BranchName, RepoSlug};

use super::gh::{gh_json, GhError};

const MAX_DEPTH: usize = 16;

/// Branches that are always protected roots in our repos. Skipping
/// the head→base walk here saves a ~500ms `gh pr list` per
/// iteration on non-stacked PRs.
const ROOT_BRANCHES: &[&str] = &["master", "main"];

#[derive(Debug, Deserialize)]
struct StackParent {
    #[serde(rename = "baseRefName")]
    base_ref_name: BranchName,
}

/// Walk down the stack from `start_branch` and return the ultimate
/// base. For non-stacked PRs (start = master/main), returns
/// `start_branch` unchanged without an API call.
pub fn resolve_stack_root(
    slug: &RepoSlug,
    start_branch: &BranchName,
) -> Result<BranchName, GhError> {
    if ROOT_BRANCHES.contains(&start_branch.as_str()) {
        return Ok(start_branch.clone());
    }
    let mut current = start_branch.clone();
    let mut visited: Vec<BranchName> = vec![current.clone()];
    let slug_s = slug.to_string();

    for _ in 0..MAX_DEPTH {
        // Re-check root inside the loop, not just at entry. Without
        // this, after following base→head once and landing on
        // master/main, we'd issue another `gh pr list --head master`,
        // which can return an unrelated PR (e.g. a release-sync PR
        // whose head IS master) and silently follow its base —
        // resulting in branch_rules / branch_protection / Copilot
        // config being fetched for the wrong branch.
        if ROOT_BRANCHES.contains(&current.as_str()) {
            return Ok(current);
        }
        let result: Vec<StackParent> = gh_json(&[
            "pr",
            "list",
            "-R",
            &slug_s,
            "--head",
            current.as_str(),
            "--state",
            "open",
            "--json",
            "baseRefName",
            "--limit",
            "1",
        ])?;
        let Some(first) = result.into_iter().next() else {
            // No open PR with `current` as head → we're at the root.
            return Ok(current);
        };
        if visited.contains(&first.base_ref_name) {
            // Cycle (shouldn't happen, but bail safely).
            return Ok(current);
        }
        visited.push(first.base_ref_name.clone());
        current = first.base_ref_name;
    }
    Ok(current)
}
