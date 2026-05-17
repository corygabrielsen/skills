//! Doc-hygiene attestation axis. Pure projection of an attestation
//! observation.
//!
//! # Invariants
//!
//! - **Sync iff SHA-equal**: an attestation paired with a HEAD that
//!   matches its recorded SHA is the only Synced witness; every other
//!   case is drift or absence.
//! - **Distance is hint, not gate**: drift carries an optional commit
//!   count for prompt enrichment, but the classification is driven by
//!   SHA inequality — an unknown count still classifies as Drift.
//! - **Distinct namespace from sibling sync axes**: this axis tracks
//!   a separate claim (doc/comment hygiene at a SHA) with its own
//!   schema version, so an unrelated axis's attestation never
//!   satisfies this gate.

use serde::Serialize;

use crate::observe::github::doc_review_attest::DocReviewObservation;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum DocReview {
    Synced,
    Drift {
        attested_sha: String,
        head_sha: String,
        commits_behind: Option<usize>,
    },
    NeverAttested,
}

/// Project a `DocReviewObservation` into the typed axis.
#[must_use]
pub(crate) fn orient_doc_review(obs: &DocReviewObservation) -> DocReview {
    match &obs.attestation {
        None => DocReview::NeverAttested,
        Some(att) if att.attested_sha == obs.head_sha.as_str() => DocReview::Synced,
        Some(att) => DocReview::Drift {
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
    use ooda_core::attest::{DOC_REVIEW_SCHEMA_VERSION, DocReviewAttestation};

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn attestation(sha: &str) -> DocReviewAttestation {
        DocReviewAttestation {
            attested_sha: sha.to_string(),
            attested_at: Utc::now(),
            version: DOC_REVIEW_SCHEMA_VERSION,
        }
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }

    #[test]
    fn no_attestation_yields_never_attested() {
        let obs = DocReviewObservation {
            attestation: None,
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        assert_eq!(orient_doc_review(&obs), DocReview::NeverAttested);
    }

    #[test]
    fn matching_sha_yields_synced() {
        let obs = DocReviewObservation {
            attestation: Some(attestation(HEAD_SHA)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        assert_eq!(orient_doc_review(&obs), DocReview::Synced);
    }

    #[test]
    fn mismatched_sha_with_count_yields_drift() {
        let obs = DocReviewObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: Some(3),
            attest_path: None,
        };
        assert_eq!(
            orient_doc_review(&obs),
            DocReview::Drift {
                attested_sha: OTHER_SHA.to_string(),
                head_sha: HEAD_SHA.to_string(),
                commits_behind: Some(3),
            }
        );
    }

    #[test]
    fn mismatched_sha_with_none_count_preserves_unknown() {
        let obs = DocReviewObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: None,
            attest_path: None,
        };
        match orient_doc_review(&obs) {
            DocReview::Drift { commits_behind, .. } => assert_eq!(commits_behind, None),
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_attestation_classifies_as_synced() {
        use crate::ids::{PullRequestNumber, RepoSlug};
        use crate::observe::github::doc_review_attest::{
            doc_review_attest_path, observe_doc_review,
        };
        use ooda_core::attest::write_doc_review_atomic;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pr = PullRequestNumber::parse("753").unwrap();
        let slug = RepoSlug::parse("acme/widget").unwrap();
        let head_sha = GitCommitSha::parse(HEAD_SHA).unwrap();

        write_doc_review_atomic(
            &doc_review_attest_path(dir.path(), pr),
            HEAD_SHA.to_string(),
        )
        .unwrap();

        let obs = observe_doc_review(Some(dir.path()), &slug, pr, &head_sha);
        assert_eq!(orient_doc_review(&obs), DocReview::Synced);
    }

    #[test]
    fn mismatched_sha_with_zero_count_still_drift() {
        let obs = DocReviewObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            commits_behind: Some(0),
            attest_path: None,
        };
        match orient_doc_review(&obs) {
            DocReview::Drift {
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
