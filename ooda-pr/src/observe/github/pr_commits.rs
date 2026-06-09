//! `GET /repos/{slug}/pulls/{n}/commits` — per-commit signing
//! verification.
//!
//! Returns the verification record carried natively by the GitHub
//! commits endpoint (`commit.verification.{verified, reason}`) so
//! the `SigningEligibility` axis can closure-check the absence of
//! valid signatures on a branch that requires them.
//!
//! One paginated call (`per_page=250`) covers every PR we'll
//! observe in practice; PRs with more than 250 commits are not in
//! scope for the closure-check (rule-of-3 — saturate first, lift
//! pagination later if evidence warrants).

use super::gh::{GhError, gh_json};
use crate::ids::{GitCommitSha, PullRequestNumber, RepoSlug};
use serde::{Deserialize, Serialize};

/// Single commit row from the commits endpoint. Only the fields
/// the closure check needs survive — everything else is discarded
/// at deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrCommit {
    pub sha: GitCommitSha,
    pub commit: CommitInner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommitInner {
    pub verification: CommitVerification,
}

/// Verification record. `verified` is the load-bearing bool; the
/// closure check treats any non-`verified` commit on a signing-
/// required branch as pathology. `reason` is carried for the
/// prompt's witness body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommitVerification {
    pub verified: bool,
    /// GitHub's enumeration: `valid` / `unsigned` / `no_user` /
    /// `bad_email` / `unknown_signature_type` / `unknown_key` /
    /// `unsigned_self` / `malformed_signature` / and more. Treated
    /// as opaque here; the axis displays it verbatim in the handoff.
    pub reason: String,
}

/// Fetch every commit on the PR with its verification record.
/// Single paginated REST call.
pub(crate) fn fetch_pr_commits(
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<Vec<PrCommit>, GhError> {
    let path = format!("repos/{slug}/pulls/{pr}/commits?per_page=250");
    gh_json(&["api", &path])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_minimal_commit_row() {
        let json = r#"[{
            "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "commit": {
                "verification": {
                    "verified": true,
                    "reason": "valid"
                }
            }
        }]"#;
        let rows: Vec<PrCommit> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].commit.verification.verified);
        assert_eq!(rows[0].commit.verification.reason, "valid");
    }

    #[test]
    fn unsigned_commit_deserializes_with_unsigned_reason() {
        let json = r#"[{
            "sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "commit": {
                "verification": {
                    "verified": false,
                    "reason": "unsigned"
                }
            }
        }]"#;
        let rows: Vec<PrCommit> = serde_json::from_str(json).unwrap();
        assert!(!rows[0].commit.verification.verified);
        assert_eq!(rows[0].commit.verification.reason, "unsigned");
    }

    #[test]
    fn discards_unknown_top_level_fields() {
        // GitHub's commits payload includes author, committer,
        // parents, message, url, etc. The PrCommit projection
        // ignores them at deserialization — we only need sha and
        // verification.
        let json = r#"[{
            "sha": "cccccccccccccccccccccccccccccccccccccccc",
            "url": "https://api.github.com/repos/x/y/commits/cccc",
            "author": {"login": "alice"},
            "parents": [],
            "commit": {
                "message": "Fix the bug",
                "author": {"name": "Alice", "email": "a@x"},
                "verification": {
                    "verified": true,
                    "reason": "valid",
                    "signature": "...",
                    "payload": "..."
                }
            }
        }]"#;
        let rows: Vec<PrCommit> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].commit.verification.verified);
    }
}
