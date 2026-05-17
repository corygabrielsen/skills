//! Observation for the doc-hygiene attestation axis.
//!
//! Same protocol shape as the sibling SHA-keyed attestation
//! observation — attestation read, optional compare-distance query
//! on SHA mismatch, malformed-file degradation to absence. Schema
//! namespace is distinct so an attestation from one axis never
//! satisfies the other.

use std::path::PathBuf;

use ooda_core::attest::{DocReviewAttestation, read_doc_review};
use serde::Serialize;

use crate::ids::{GitCommitSha, PullRequestNumber, RepoSlug};

use super::gh::{GhError, encode_path_segment, gh_json};

const DOC_REVIEW_FILE: &str = "doc_review_attest.json";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DocReviewObservation {
    pub attestation: Option<DocReviewAttestation>,
    pub head_sha: GitCommitSha,
    pub commits_behind: Option<usize>,
    pub attest_path: Option<PathBuf>,
}

/// Compose the attestation file path. Shared with the prompt-
/// composition layer so the agent receives the same absolute path
/// it must record against.
#[must_use]
pub(crate) fn doc_review_attest_path(
    state_root: &std::path::Path,
    pr: PullRequestNumber,
) -> PathBuf {
    state_root.join(pr.to_string()).join(DOC_REVIEW_FILE)
}

/// Read the attestation plus the optional drift distance against
/// the current HEAD.
pub(crate) fn observe_doc_review(
    state_root: Option<&std::path::Path>,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    head_sha: &GitCommitSha,
) -> DocReviewObservation {
    let path = state_root.map(|root| doc_review_attest_path(root, pr));
    let attestation = path
        .as_deref()
        .and_then(|p| read_doc_review(p).ok().flatten());
    let commits_behind = match &attestation {
        Some(att) if att.attested_sha != head_sha.as_str() => {
            compare_ahead_by(slug, &att.attested_sha, head_sha)
        }
        _ => None,
    };
    DocReviewObservation {
        attestation,
        head_sha: head_sha.clone(),
        commits_behind,
        attest_path: path,
    }
}

fn compare_ahead_by(slug: &RepoSlug, attested_sha: &str, head: &GitCommitSha) -> Option<usize> {
    let envelope = fetch_compare_envelope(slug, attested_sha, head).ok()?;
    Some(envelope.ahead_by as usize)
}

#[derive(serde::Deserialize)]
struct CompareEnvelope {
    #[serde(default)]
    ahead_by: u32,
}

fn fetch_compare_envelope(
    slug: &RepoSlug,
    attested_sha: &str,
    head: &GitCommitSha,
) -> Result<CompareEnvelope, GhError> {
    let path = format!(
        "repos/{slug}/compare/{}...{}",
        encode_path_segment(attested_sha),
        head.as_str(),
    );
    gh_json::<CompareEnvelope>(&["api", &path])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ooda_core::attest::{DOC_REVIEW_SCHEMA_VERSION, write_doc_review_atomic};
    use tempfile::tempdir;

    const VALID_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn slug() -> RepoSlug {
        RepoSlug::parse("acme/widget").unwrap()
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(VALID_SHA).unwrap()
    }

    #[test]
    fn attest_path_joins_pull_request_id_and_filename() {
        let p = doc_review_attest_path(std::path::Path::new("/state"), pr());
        assert_eq!(
            p,
            std::path::PathBuf::from("/state/753/doc_review_attest.json")
        );
    }

    #[test]
    fn missing_state_root_yields_no_attestation_and_no_compare() {
        let obs = observe_doc_review(None, &slug(), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.commits_behind.is_none());
        assert_eq!(obs.head_sha, head());
    }

    #[test]
    fn missing_attestation_file_yields_none_without_touching_gh() {
        let dir = tempdir().unwrap();
        let obs = observe_doc_review(Some(dir.path()), &slug(), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.commits_behind.is_none());
    }

    #[test]
    fn attestation_matching_head_yields_no_commits_behind_query() {
        let dir = tempdir().unwrap();
        let path = doc_review_attest_path(dir.path(), pr());
        write_doc_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs = observe_doc_review(Some(dir.path()), &slug(), pr(), &head());
        let att = obs.attestation.expect("attestation should be present");
        assert_eq!(att.attested_sha, VALID_SHA);
        assert_eq!(att.version, DOC_REVIEW_SCHEMA_VERSION);
        assert!(obs.commits_behind.is_none());
    }

    #[test]
    fn malformed_attestation_file_degrades_to_none() {
        let dir = tempdir().unwrap();
        let path = doc_review_attest_path(dir.path(), pr());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let obs = observe_doc_review(Some(dir.path()), &slug(), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.commits_behind.is_none());
    }

    #[test]
    fn round_trip_observation_round_trips_attested_sha() {
        let dir = tempdir().unwrap();
        let path = doc_review_attest_path(dir.path(), pr());
        let written = write_doc_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs = observe_doc_review(Some(dir.path()), &slug(), pr(), &head());
        assert_eq!(obs.attestation.as_ref().unwrap(), &written);
    }

    #[test]
    fn doc_review_observation_serializes_with_typed_fields() {
        let obs = DocReviewObservation {
            attestation: Some(DocReviewAttestation {
                attested_sha: VALID_SHA.to_string(),
                attested_at: Utc::now(),
                version: DOC_REVIEW_SCHEMA_VERSION,
            }),
            head_sha: GitCommitSha::parse(OTHER_SHA).unwrap(),
            commits_behind: Some(3),
            attest_path: Some(std::path::PathBuf::from(
                "/state/753/doc_review_attest.json",
            )),
        };
        let json = serde_json::to_string(&obs).unwrap();
        assert!(json.contains(VALID_SHA));
        assert!(json.contains(OTHER_SHA));
        assert!(json.contains("\"commits_behind\":3"));
        assert!(json.contains("/state/753/doc_review_attest.json"));
    }

    #[test]
    fn observation_attest_path_present_when_state_root_supplied() {
        let dir = tempdir().unwrap();
        let obs = observe_doc_review(Some(dir.path()), &slug(), pr(), &head());
        let path = obs.attest_path.expect("path should be present");
        assert!(path.ends_with("753/doc_review_attest.json"));
    }

    #[test]
    fn observation_attest_path_absent_when_state_root_missing() {
        let obs = observe_doc_review(None, &slug(), pr(), &head());
        assert!(obs.attest_path.is_none());
    }
}
