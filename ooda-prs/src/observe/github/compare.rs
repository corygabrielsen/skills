//! Typed view of `GET /repos/{o}/{r}/compare/{base}...{head}`.
//!
//! The compare endpoint is the only source for merge-base-relative
//! facts: how many commits the PR is behind, which files master
//! touched since the merge base, and (by intersection with the
//! branch's own diff) the empty-or-non-empty conflict surface.
//!
//! `gh pr view` exposes only the `behind` enum bit, not a count or
//! a file list — both required to enrich the Rebase prompt with a
//! concrete recommendation rather than a generic "rebase now."
//
// This module is the first piece of "compare endpoint" data the
// loop observes. Future questions about ancestor state — e.g. how
// many merge conflicts would actually surface, which commits the
// branch is ahead by, base-commit author churn — extend
// `MergeBaseDelta` rather than spawning sibling structs. One fetch,
// one observation, one place to grow.

use serde::{Deserialize, Serialize};

use crate::ids::{BranchName, GitCommitSha, RepoSlug, Timestamp};

use super::gh::{GhError, encode_path_segment, gh_json};

/// Merge-base-relative delta between a PR's head and its base. All
/// counts and file lists are computed by GitHub against the *merge
/// base*, not the tip of base, so they describe exactly the work a
/// rebase would replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MergeBaseDelta {
    /// Count of commits on base since the merge base.
    pub commits_behind: u32,
    /// Count of commits on the branch since the merge base.
    pub commits_ahead: u32,
    /// Files touched on the base side of the compare (commits on
    /// base since the merge base, unioned across all commits).
    pub master_files: Vec<String>,
    /// Files touched on the branch side of the compare.
    pub branch_files: Vec<String>,
    /// Intersection of `master_files` and `branch_files`. Computed
    /// at observation time so consumers don't need to recompute the
    /// set everywhere it matters.
    pub conflict_surface: Vec<String>,
    /// Author timestamp of the oldest commit on base since the merge
    /// base — "behind since when." `None` when `commits_behind == 0`
    /// or when no commit carries an author date.
    pub oldest_master_commit_at: Option<Timestamp>,
}

// Wire shapes for `/repos/{o}/{r}/compare/{base}...{head}`. We
// deserialize only the fields `MergeBaseDelta` consumes; the
// endpoint also returns merge_base_commit, total_commits, status,
// permalink_url etc. — adding those is a strictly additive
// extension when a consumer arrives.

#[derive(Debug, Clone, Deserialize)]
struct CompareEnvelope {
    #[serde(default)]
    behind_by: u32,
    #[serde(default)]
    ahead_by: u32,
    #[serde(default)]
    commits: Vec<CompareCommitWire>,
    #[serde(default)]
    files: Vec<CompareFileWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct CompareCommitWire {
    #[serde(default)]
    commit: Option<CommitInner>,
    #[serde(default)]
    files: Option<Vec<CompareFileWire>>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitInner {
    #[serde(default)]
    author: Option<CommitAuthorRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitAuthorRef {
    #[serde(default)]
    date: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CompareFileWire {
    filename: String,
}

/// Fetch the compare-endpoint delta between `head` and `base`.
///
/// GitHub computes the comparison from the *merge base* of the two
/// refs, so `behind_by` / `ahead_by` and the file lists describe
/// exactly the divergence a rebase would replay.
///
/// `gh api compare` returns `files` aggregated across the branch
/// side. The base-side file list lives inside `commits[].files` —
/// the v3 `commits[]` array only includes a `files` payload when
/// fetched with the appropriate Accept header; when absent, we
/// approximate `master_files` as empty and `conflict_surface` falls
/// back to empty as well. This is the rare path; the typical path
/// returns the per-commit file list inline.
pub(crate) fn fetch_merge_base_delta(
    slug: &RepoSlug,
    base: &BranchName,
    head: &GitCommitSha,
) -> Result<MergeBaseDelta, GhError> {
    // Three-dot syntax `{base}...{head}` asks GitHub for the merge-
    // base-relative comparison. Two-dot `{base}..{head}` would be a
    // direct ref-to-ref diff with different semantics (no behind_by).
    //
    // Branch names containing `/` (release/1.2) must remain one
    // segment — encode them so `gh api` doesn't reparse into
    // additional path components.
    let path = format!(
        "repos/{slug}/compare/{}...{}",
        encode_path_segment(base.as_str()),
        head.as_str(),
    );
    let env: CompareEnvelope = gh_json(&["api", &path])?;
    Ok(project(env))
}

fn project(env: CompareEnvelope) -> MergeBaseDelta {
    let branch_files: Vec<String> = env.files.into_iter().map(|f| f.filename).collect();

    let mut master_files: Vec<String> = Vec::new();
    let mut oldest_master_commit_at: Option<Timestamp> = None;
    for commit in &env.commits {
        if let Some(files) = &commit.files {
            for f in files {
                if !master_files.contains(&f.filename) {
                    master_files.push(f.filename.clone());
                }
            }
        }
        if let Some(inner) = &commit.commit
            && let Some(author) = &inner.author
            && let Some(date) = author.date.as_deref()
            && let Ok(ts) = Timestamp::parse(date)
        {
            oldest_master_commit_at = Some(match oldest_master_commit_at {
                Some(cur) if cur <= ts => cur,
                _ => ts,
            });
        }
    }

    let conflict_surface: Vec<String> = master_files
        .iter()
        .filter(|p| branch_files.contains(p))
        .cloned()
        .collect();

    MergeBaseDelta {
        commits_behind: env.behind_by,
        commits_ahead: env.ahead_by,
        master_files,
        branch_files,
        conflict_surface,
        oldest_master_commit_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_full_envelope_with_overlap() {
        let json = r#"{
            "behind_by": 3,
            "ahead_by": 2,
            "commits": [
                {
                    "commit": {"author": {"date": "2026-05-10T09:00:00Z"}},
                    "files": [
                        {"filename": "src/a.rs"},
                        {"filename": "src/b.rs"}
                    ]
                },
                {
                    "commit": {"author": {"date": "2026-05-12T11:00:00Z"}},
                    "files": [
                        {"filename": "src/c.rs"}
                    ]
                }
            ],
            "files": [
                {"filename": "src/b.rs"},
                {"filename": "src/d.rs"}
            ]
        }"#;
        let env: CompareEnvelope = serde_json::from_str(json).unwrap();
        let delta = project(env);
        assert_eq!(delta.commits_behind, 3);
        assert_eq!(delta.commits_ahead, 2);
        assert_eq!(delta.master_files, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert_eq!(delta.branch_files, vec!["src/b.rs", "src/d.rs"]);
        assert_eq!(delta.conflict_surface, vec!["src/b.rs"]);
        assert_eq!(
            delta.oldest_master_commit_at,
            Some(Timestamp::parse("2026-05-10T09:00:00Z").unwrap()),
        );
    }

    #[test]
    fn empty_intersection_yields_empty_conflict_surface() {
        let json = r#"{
            "behind_by": 1,
            "ahead_by": 1,
            "commits": [
                {
                    "commit": {"author": {"date": "2026-05-10T09:00:00Z"}},
                    "files": [{"filename": "docs/readme.md"}]
                }
            ],
            "files": [{"filename": "src/lib.rs"}]
        }"#;
        let env: CompareEnvelope = serde_json::from_str(json).unwrap();
        let delta = project(env);
        assert!(delta.conflict_surface.is_empty());
        assert_eq!(delta.master_files, vec!["docs/readme.md"]);
        assert_eq!(delta.branch_files, vec!["src/lib.rs"]);
    }

    #[test]
    fn missing_commit_files_payload_leaves_master_files_empty() {
        // Some accept-header / endpoint variants omit per-commit
        // files; the projection must degrade gracefully rather than
        // crashing the observe pass.
        let json = r#"{
            "behind_by": 2,
            "ahead_by": 0,
            "commits": [
                {"commit": {"author": {"date": "2026-05-10T09:00:00Z"}}}
            ],
            "files": [{"filename": "src/x.rs"}]
        }"#;
        let env: CompareEnvelope = serde_json::from_str(json).unwrap();
        let delta = project(env);
        assert_eq!(delta.commits_behind, 2);
        assert!(delta.master_files.is_empty());
        assert!(delta.conflict_surface.is_empty());
        assert_eq!(delta.branch_files, vec!["src/x.rs"]);
    }

    #[test]
    fn empty_envelope_projects_to_zeros() {
        let env: CompareEnvelope = serde_json::from_str("{}").unwrap();
        let delta = project(env);
        assert_eq!(delta.commits_behind, 0);
        assert_eq!(delta.commits_ahead, 0);
        assert!(delta.master_files.is_empty());
        assert!(delta.branch_files.is_empty());
        assert!(delta.conflict_surface.is_empty());
        assert!(delta.oldest_master_commit_at.is_none());
    }

    #[test]
    fn oldest_master_commit_at_picks_earliest_author_date() {
        // Order in the response is by topology; we pick min author
        // date so "behind since when" is the actual earliest commit.
        let json = r#"{
            "behind_by": 3,
            "commits": [
                {"commit": {"author": {"date": "2026-05-15T09:00:00Z"}}},
                {"commit": {"author": {"date": "2026-05-09T09:00:00Z"}}},
                {"commit": {"author": {"date": "2026-05-12T09:00:00Z"}}}
            ]
        }"#;
        let env: CompareEnvelope = serde_json::from_str(json).unwrap();
        let delta = project(env);
        assert_eq!(
            delta.oldest_master_commit_at,
            Some(Timestamp::parse("2026-05-09T09:00:00Z").unwrap()),
        );
    }
}
