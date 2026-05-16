//! PR-meta sync state. Pure projection of `PrMetaObservation`.
//!
//! Three states:
//! * `Synced` — an attestation exists AND its SHA equals HEAD.
//! * `Drift { attested_sha, head_sha, commits_behind }` — an
//!   attestation exists but its SHA differs from HEAD. The
//!   `commits_behind` count comes from `gh api compare`; a failed
//!   compare collapses to 0 (orient still classifies as Drift —
//!   the SHA mismatch is the trigger, not the count).
//! * `NeverAttested` — no attestation file was read (file absent,
//!   malformed, or schema-version-mismatched all collapse here).
//!
//! Distinct shape from the bot health axes (Healthy/Degraded/
//! Failed). The PR-meta axis is a sync state, not a fitness
//! tier — Drift is mechanically resolvable by re-running the
//! `ooda-attest pr-meta` CLI after updating the PR.

use serde::Serialize;

use crate::observe::github::pr_meta_attest::PrMetaObservation;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PrMetadata {
    Synced,
    Drift {
        attested_sha: String,
        head_sha: String,
        commits_behind: usize,
    },
    NeverAttested,
}

/// Project a `PrMetaObservation` into the typed axis.
#[must_use]
pub fn orient_pr_meta(obs: &PrMetaObservation) -> PrMetadata {
    match &obs.attestation {
        None => PrMetadata::NeverAttested,
        Some(att) if att.attested_sha == obs.head_sha.as_str() => PrMetadata::Synced,
        Some(att) => PrMetadata::Drift {
            attested_sha: att.attested_sha.clone(),
            head_sha: obs.head_sha.as_str().to_string(),
            commits_behind: obs.commits_behind.unwrap_or(0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitCommitSha;
    use chrono::Utc;
    use ooda_core::attest::{PR_META_SCHEMA_VERSION, PrMetaAttestation};

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn attestation(sha: &str) -> PrMetaAttestation {
        PrMetaAttestation {
            attested_sha: sha.to_string(),
            attested_at: Utc::now(),
            version: PR_META_SCHEMA_VERSION,
        }
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }

    #[test]
    fn no_attestation_yields_never_attested() {
        let obs = PrMetaObservation {
            attestation: None,
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        assert_eq!(orient_pr_meta(&obs), PrMetadata::NeverAttested);
    }

    #[test]
    fn matching_sha_yields_synced() {
        let obs = PrMetaObservation {
            attestation: Some(attestation(HEAD_SHA)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        assert_eq!(orient_pr_meta(&obs), PrMetadata::Synced);
    }

    #[test]
    fn mismatched_sha_with_count_yields_drift() {
        let obs = PrMetaObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: Some(3),
            attest_path: None,
        };
        assert_eq!(
            orient_pr_meta(&obs),
            PrMetadata::Drift {
                attested_sha: OTHER_SHA.to_string(),
                head_sha: HEAD_SHA.to_string(),
                commits_behind: 3,
            }
        );
    }

    #[test]
    fn mismatched_sha_with_none_count_yields_drift_with_zero() {
        let obs = PrMetaObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        match orient_pr_meta(&obs) {
            PrMetadata::Drift { commits_behind, .. } => assert_eq!(commits_behind, 0),
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_attestation_classifies_as_synced() {
        use crate::ids::{PullRequestNumber, RepoSlug};
        use crate::observe::github::pr_meta_attest::{attest_path, observe_pr_meta};
        use ooda_core::attest::write_pr_meta_atomic;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pr = PullRequestNumber::parse("753").unwrap();
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let head_sha = GitCommitSha::parse(HEAD_SHA).unwrap();

        write_pr_meta_atomic(&attest_path(dir.path(), pr), HEAD_SHA.to_string()).unwrap();

        let obs = observe_pr_meta(Some(dir.path()), &slug, pr, &head_sha);
        assert_eq!(orient_pr_meta(&obs), PrMetadata::Synced);
    }

    #[test]
    fn mismatched_sha_with_zero_count_still_drift() {
        let obs = PrMetaObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: Some(0),
            attest_path: None,
        };
        match orient_pr_meta(&obs) {
            PrMetadata::Drift {
                attested_sha,
                head_sha,
                commits_behind,
            } => {
                assert_eq!(attested_sha, OTHER_SHA);
                assert_eq!(head_sha, HEAD_SHA);
                assert_eq!(commits_behind, 0);
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }
}
