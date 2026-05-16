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

use super::gh::{GhError, gh_json};

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
        match advance_root(current.clone(), result, &mut visited) {
            AdvanceStep::Reached(branch) => return Ok(branch),
            AdvanceStep::Continue(next) => current = next,
        }
    }
    Ok(current)
}

/// Result of one stack-walk step. Split out from the gh-bound loop
/// body so cycle detection, empty-list termination, and the
/// "advance to parent" branch are unit-testable without subprocesses.
#[derive(Debug, PartialEq, Eq)]
enum AdvanceStep {
    /// Terminal: stop walking and return this branch.
    Reached(BranchName),
    /// Non-terminal: continue the walk with this branch as the new
    /// current head.
    Continue(BranchName),
}

fn advance_root(
    current: BranchName,
    parents: Vec<StackParent>,
    visited: &mut Vec<BranchName>,
) -> AdvanceStep {
    let Some(first) = parents.into_iter().next() else {
        // No open PR with `current` as head → we're at the root.
        return AdvanceStep::Reached(current);
    };
    if visited.contains(&first.base_ref_name) {
        // Cycle (shouldn't happen, but bail safely).
        return AdvanceStep::Reached(current);
    }
    visited.push(first.base_ref_name.clone());
    AdvanceStep::Continue(first.base_ref_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str) -> BranchName {
        BranchName::parse(name).unwrap()
    }

    fn parent(base: &str) -> StackParent {
        StackParent {
            base_ref_name: branch(base),
        }
    }

    // ── resolve_stack_root entry-time root short-circuit ──

    #[test]
    fn resolve_returns_master_immediately_without_gh_call() {
        // Calling resolve_stack_root with `master` returns Ok
        // synchronously — the early-return short-circuits the gh
        // subprocess. The test verifies it does not panic / error
        // (any subprocess attempt would fail in the test sandbox).
        let slug = RepoSlug::parse("acme/widgets").unwrap();
        let out = resolve_stack_root(&slug, &branch("master")).unwrap();
        assert_eq!(out, branch("master"));
    }

    #[test]
    fn resolve_returns_main_immediately_without_gh_call() {
        let slug = RepoSlug::parse("acme/widgets").unwrap();
        let out = resolve_stack_root(&slug, &branch("main")).unwrap();
        assert_eq!(out, branch("main"));
    }

    // ── advance_root inner step (pure) ──

    #[test]
    fn advance_root_reaches_root_when_no_parents() {
        let mut visited = vec![branch("feature/x")];
        let step = advance_root(branch("feature/x"), vec![], &mut visited);
        assert_eq!(step, AdvanceStep::Reached(branch("feature/x")));
    }

    #[test]
    fn advance_root_continues_to_parent_branch() {
        let mut visited = vec![branch("feature/x")];
        let step = advance_root(
            branch("feature/x"),
            vec![parent("feature/parent")],
            &mut visited,
        );
        assert_eq!(step, AdvanceStep::Continue(branch("feature/parent")));
        assert!(visited.contains(&branch("feature/parent")));
    }

    #[test]
    fn advance_root_bails_on_cycle() {
        // visited already contains the proposed parent → cycle guard
        // returns the current branch instead of advancing into a loop.
        let mut visited = vec![branch("a"), branch("b"), branch("c")];
        let step = advance_root(branch("c"), vec![parent("a")], &mut visited);
        assert_eq!(step, AdvanceStep::Reached(branch("c")));
    }

    #[test]
    fn advance_root_takes_first_parent_when_multiple() {
        // `--limit 1` constrains gh to return at most one parent,
        // but defensively the helper takes the first if multiple
        // ever arrive on the wire.
        let mut visited = vec![branch("feature/x")];
        let step = advance_root(
            branch("feature/x"),
            vec![parent("feature/first"), parent("feature/second")],
            &mut visited,
        );
        assert_eq!(step, AdvanceStep::Continue(branch("feature/first")));
    }
}
