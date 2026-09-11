//! Observation for the review-class attestation axis.
//!
//! Same protocol shape as the sibling attestation observations:
//! attestation read, malformed-file degradation to absence. No
//! compare-distance query — the axis is content-keyed (thread
//! creation times against `attested_at`), so HEAD distance carries
//! no signal. The threads themselves are already in the bundle;
//! orient joins them.

use std::path::PathBuf;

use ooda_core::attest::{ReviewClassAttestation, read_review_class};
use serde::Serialize;

use crate::ids::{GitCommitSha, PullRequestNumber};

const REVIEW_CLASS_FILE: &str = "review_class_attest.json";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReviewClassObservation {
    pub attestation: Option<ReviewClassAttestation>,
    pub head_sha: GitCommitSha,
    pub attest_path: Option<PathBuf>,
}

/// Compose the attestation file path. Shared with the prompt-
/// composition layer so the agent receives the same absolute path
/// it must record against.
#[must_use]
pub(crate) fn review_class_attest_path(
    state_root: &std::path::Path,
    pr: PullRequestNumber,
) -> PathBuf {
    state_root.join(pr.to_string()).join(REVIEW_CLASS_FILE)
}

/// Read the attestation. Absent state-root degrades to "no
/// attestation possible" without touching the filesystem.
pub(crate) fn observe_review_class(
    state_root: Option<&std::path::Path>,
    pr: PullRequestNumber,
    head_sha: &GitCommitSha,
) -> ReviewClassObservation {
    let path = state_root.map(|root| review_class_attest_path(root, pr));
    let attestation = path
        .as_deref()
        .and_then(|p| read_review_class(p).ok().flatten());
    ReviewClassObservation {
        attestation,
        head_sha: head_sha.clone(),
        attest_path: path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooda_core::attest::{
        REVIEW_CLASS_SCHEMA_VERSION, ReviewClassEntry, ReviewSite, write_review_class_atomic,
    };
    use tempfile::tempdir;

    const VALID_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(VALID_SHA).unwrap()
    }

    fn classes() -> Vec<ReviewClassEntry> {
        vec![ReviewClassEntry {
            class: "unwrap in library code".into(),
            sites: vec![ReviewSite {
                path: "src/a.rs".into(),
                line: 12,
            }],
        }]
    }

    #[test]
    fn attest_path_joins_pull_request_id_and_filename() {
        let p = review_class_attest_path(std::path::Path::new("/state"), pr());
        assert_eq!(
            p,
            std::path::PathBuf::from("/state/753/review_class_attest.json")
        );
    }

    #[test]
    fn missing_state_root_yields_no_attestation() {
        let obs = observe_review_class(None, pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.attest_path.is_none());
        assert_eq!(obs.head_sha, head());
    }

    #[test]
    fn missing_attestation_file_yields_none() {
        let dir = tempdir().unwrap();
        let obs = observe_review_class(Some(dir.path()), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.attest_path.is_some());
    }

    #[test]
    fn malformed_attestation_file_degrades_to_none() {
        let dir = tempdir().unwrap();
        let path = review_class_attest_path(dir.path(), pr());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let obs = observe_review_class(Some(dir.path()), pr(), &head());
        assert!(obs.attestation.is_none());
    }

    #[test]
    fn round_trip_attestation_reads_back() {
        let dir = tempdir().unwrap();
        let path = review_class_attest_path(dir.path(), pr());
        let written = write_review_class_atomic(&path, VALID_SHA.to_string(), classes()).unwrap();
        let obs = observe_review_class(Some(dir.path()), pr(), &head());
        let att = obs.attestation.expect("attestation present");
        assert_eq!(att, written);
        assert_eq!(att.version, REVIEW_CLASS_SCHEMA_VERSION);
        assert_eq!(att.classes.len(), 1);
    }
}
