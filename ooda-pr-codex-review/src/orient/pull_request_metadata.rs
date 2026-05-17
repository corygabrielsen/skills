//! PR-metadata attestation axis. Pure projection of an attestation
//! observation.
//!
//! # Invariants
//!
//! - **Sync iff SHA-equal**: a Synced witness is an attestation
//!   recorded against the current HEAD. Every other case is drift or
//!   absence.
//! - **Distance is hint, not gate**: drift carries an optional commit
//!   count for prompt enrichment, but the classification is driven by
//!   SHA inequality — an unknown count (compare failure, pruned SHA)
//!   still classifies as Drift.
//! - **Absence is structural, not exceptional**: malformed file,
//!   schema-version skew, missing file all collapse to a single
//!   `NeverAttested` variant. Decide handles "first time" identically
//!   to "schema rotated"; this axis does not differentiate.
//! - **Sync axis, not fitness axis**: drift is mechanically
//!   resolvable by re-recording at the current HEAD — there is no
//!   degraded/failed lattice and no remediation budget.

use serde::Serialize;

use crate::observe::github::pull_request_metadata_attestation::PullRequestMetadataObservation;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum PullRequestMetadata {
    Synced,
    Drift {
        attested_sha: String,
        head_sha: String,
        commits_behind: Option<usize>,
    },
    NeverAttested,
}

/// Project a `PullRequestMetadataObservation` into the typed axis.
#[must_use]
pub(crate) fn orient_pull_request_metadata(
    obs: &PullRequestMetadataObservation,
) -> PullRequestMetadata {
    match &obs.attestation {
        None => PullRequestMetadata::NeverAttested,
        Some(att) if att.attested_sha == obs.head_sha.as_str() => PullRequestMetadata::Synced,
        Some(att) => PullRequestMetadata::Drift {
            attested_sha: att.attested_sha.clone(),
            head_sha: obs.head_sha.as_str().to_string(),
            commits_behind: obs.commits_behind,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitCommitSha;
    use chrono::Utc;
    use ooda_core::attest::{PULL_REQUEST_METADATA_SCHEMA_VERSION, PullRequestMetadataAttestation};

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn attestation(sha: &str) -> PullRequestMetadataAttestation {
        PullRequestMetadataAttestation {
            attested_sha: sha.to_string(),
            attested_at: Utc::now(),
            version: PULL_REQUEST_METADATA_SCHEMA_VERSION,
        }
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }

    #[test]
    fn no_attestation_yields_never_attested() {
        let obs = PullRequestMetadataObservation {
            attestation: None,
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        assert_eq!(
            orient_pull_request_metadata(&obs),
            PullRequestMetadata::NeverAttested
        );
    }

    #[test]
    fn matching_sha_yields_synced() {
        let obs = PullRequestMetadataObservation {
            attestation: Some(attestation(HEAD_SHA)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        assert_eq!(
            orient_pull_request_metadata(&obs),
            PullRequestMetadata::Synced
        );
    }

    #[test]
    fn mismatched_sha_with_count_yields_drift() {
        let obs = PullRequestMetadataObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: Some(3),
            attest_path: None,
        };
        assert_eq!(
            orient_pull_request_metadata(&obs),
            PullRequestMetadata::Drift {
                attested_sha: OTHER_SHA.to_string(),
                head_sha: HEAD_SHA.to_string(),
                commits_behind: Some(3),
            }
        );
    }

    #[test]
    fn mismatched_sha_with_none_count_preserves_unknown() {
        let obs = PullRequestMetadataObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        match orient_pull_request_metadata(&obs) {
            PullRequestMetadata::Drift { commits_behind, .. } => assert_eq!(commits_behind, None),
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_attestation_classifies_as_synced() {
        use crate::ids::{PullRequestNumber, RepoSlug};
        use crate::observe::github::pull_request_metadata_attestation::{
            attest_path, observe_pull_request_metadata,
        };
        use ooda_core::attest::write_pull_request_metadata_atomic;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pr = PullRequestNumber::parse("753").unwrap();
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let head_sha = GitCommitSha::parse(HEAD_SHA).unwrap();

        write_pull_request_metadata_atomic(&attest_path(dir.path(), pr), HEAD_SHA.to_string())
            .unwrap();

        let obs = observe_pull_request_metadata(Some(dir.path()), &slug, pr, &head_sha);
        assert_eq!(
            orient_pull_request_metadata(&obs),
            PullRequestMetadata::Synced
        );
    }

    #[test]
    fn mismatched_sha_with_zero_count_still_drift() {
        let obs = PullRequestMetadataObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: Some(0),
            attest_path: None,
        };
        match orient_pull_request_metadata(&obs) {
            PullRequestMetadata::Drift {
                attested_sha,
                head_sha,
                commits_behind,
            } => {
                assert_eq!(attested_sha, OTHER_SHA);
                assert_eq!(head_sha, HEAD_SHA);
                assert_eq!(commits_behind, Some(0));
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }
}
