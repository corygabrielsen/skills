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
    /// `None` when the host omits the verification record
    /// entirely. Treated by consumers as "unverified" — the
    /// closure check fires on absence the same way it fires on
    /// `verified=false`. Without `#[serde(default)]`, a single
    /// missing field on one commit would abort the entire fetch
    /// and crash the observe pass.
    #[serde(default)]
    pub verification: Option<CommitVerification>,
}

/// Verification record. `verified` is the load-bearing bool; the
/// closure check treats any non-`verified` commit on a signing-
/// required branch as pathology. `reason` is carried for the
/// prompt's witness body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommitVerification {
    pub verified: bool,
    /// Host enumeration: `valid` / `unsigned` / `no_user` /
    /// `bad_email` / `unknown_signature_type` / `unknown_key` /
    /// `unsigned_self` / `malformed_signature` / and more. Treated
    /// as opaque here; the axis displays it verbatim in the handoff.
    #[serde(default)]
    pub reason: String,
}

impl PrCommit {
    /// `true` when the commit is verified-signed. Absent
    /// verification record is treated as unverified — closure-check
    /// semantic: absence is not "no opinion", it's "not verified".
    #[must_use]
    pub(crate) fn verified(&self) -> bool {
        self.commit
            .verification
            .as_ref()
            .is_some_and(|v| v.verified)
    }
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
        assert!(rows[0].verified());
        let v = rows[0].commit.verification.as_ref().unwrap();
        assert_eq!(v.reason, "valid");
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
        assert!(!rows[0].verified());
        assert_eq!(
            rows[0].commit.verification.as_ref().unwrap().reason,
            "unsigned"
        );
    }

    #[test]
    fn discards_unknown_top_level_fields() {
        // The host's commits payload includes author, committer,
        // parents, message, url, etc. The PrCommit projection
        // ignores them at deserialization — we only need sha and
        // verification.
        let json = r#"[{
            "sha": "cccccccccccccccccccccccccccccccccccccccc",
            "url": "https://api.example/repos/x/y/commits/cccc",
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
        assert!(rows[0].verified());
    }

    #[test]
    fn missing_verification_record_deserializes_as_unverified() {
        // A commit row with `commit: {}` and no verification field.
        // Pre-fix, this aborted the entire fetch with a parse error
        // and crashed the observe pass. Post-fix, the row
        // deserializes; the consumer treats it as unverified.
        let json = r#"[{
            "sha": "dddddddddddddddddddddddddddddddddddddddd",
            "commit": {}
        }]"#;
        let rows: Vec<PrCommit> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].verified());
        assert!(rows[0].commit.verification.is_none());
    }

    #[test]
    fn one_commit_with_missing_verification_does_not_abort_batch() {
        // The whole point of the Option: one malformed row in a
        // batch of 250 must not crash the others.
        let json = r#"[
            {"sha": "1111111111111111111111111111111111111111",
             "commit": {"verification": {"verified": true, "reason": "valid"}}},
            {"sha": "2222222222222222222222222222222222222222",
             "commit": {}},
            {"sha": "3333333333333333333333333333333333333333",
             "commit": {"verification": {"verified": false, "reason": "unsigned"}}}
        ]"#;
        let rows: Vec<PrCommit> = serde_json::from_str(json).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].verified());
        assert!(!rows[1].verified()); // absent verification → unverified
        assert!(!rows[2].verified());
    }

    #[test]
    fn missing_reason_field_defaults_to_empty_string() {
        // Defensive: if the host returns `verification` but omits
        // `reason`, fall back to "" rather than crash.
        let json = r#"[{
            "sha": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "commit": {
                "verification": {"verified": false}
            }
        }]"#;
        let rows: Vec<PrCommit> = serde_json::from_str(json).unwrap();
        let v = rows[0].commit.verification.as_ref().unwrap();
        assert!(!v.verified);
        assert_eq!(v.reason, "");
    }
}
