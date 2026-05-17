//! Resolve a stacked PR's protected root by walking the open-PR
//! head→base chain.
//!
//! # Invariants
//!
//! - **Branch-rule scoping requires the protected root**: rule and
//!   protection sources resolve against the protected root, not
//!   intermediate stack branches. Querying an intermediate yields an
//!   empty rule set and silently disables the required-check axis.
//! - **Termination**: the walk ends when no open PR claims the
//!   current branch as its head. Cycles are impossible on the
//!   acyclic open-PR head→base graph; a defensive iteration cap
//!   bounds the walk regardless.
//! - **Per-step root re-check**: the root predicate is evaluated
//!   inside the loop, not only at entry. Without this, following
//!   base→head once onto a root branch could then chase an
//!   unrelated PR rooted at the same head and resolve the wrong
//!   branch.

use serde::Deserialize;

use crate::ids::{BranchName, RepoSlug};

use super::gh::{GhError, gh_json};

const MAX_DEPTH: usize = 16;

/// Branch names treated as protected roots without further probing.
/// Conventional defaults; short-circuits the head→base walk on non-
/// stacked PRs.
const ROOT_BRANCHES: &[&str] = &["master", "main"];

#[derive(Debug, Deserialize)]
struct StackParent {
    #[serde(rename = "baseRefName")]
    base_ref_name: BranchName,
}

/// Resolve the protected root reachable from `start_branch` along
/// the open-PR head→base chain. Returns `start_branch` unchanged
/// when it already names a protected root; no API call in that case.
pub(crate) fn resolve_stack_root(
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
        // Per-step root check: enforces the per-step-recheck
        // invariant in the module-level doc.
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

/// Per-step outcome. Split from the subprocess-bound loop body so
/// the termination, cycle, and advancement branches are unit-
/// testable without subprocess.
#[derive(Debug, PartialEq, Eq)]
enum AdvanceStep {
    /// Terminal — return this branch.
    Reached(BranchName),
    /// Non-terminal — continue with this branch.
    Continue(BranchName),
}

fn advance_root(
    current: BranchName,
    parents: Vec<StackParent>,
    visited: &mut Vec<BranchName>,
) -> AdvanceStep {
    let Some(first) = parents.into_iter().next() else {
        // No parent → current is the root.
        return AdvanceStep::Reached(current);
    };
    if visited.contains(&first.base_ref_name) {
        // Cycle guard — pathologically not reachable on the open-PR
        // graph, kept defensively.
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
