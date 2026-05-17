//! Observation for the final sign-off attestation axis.
//!
//! Diverges from sibling SHA-keyed observations: drift is the SHA-
//! inequality bit alone — no distance metric, no compare call. The
//! handoff prompt is a fresh full read, not an incremental review.

use std::path::PathBuf;

use ooda_core::attest::{CloseoutAttestation, read_closeout};
use serde::Serialize;

use crate::ids::{GitCommitSha, PullRequestNumber};

const CLOSEOUT_FILE: &str = "closeout_attest.json";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CloseoutObservation {
    pub attestation: Option<CloseoutAttestation>,
    pub head_sha: GitCommitSha,
    pub attest_path: Option<PathBuf>,
}

/// Compose the attestation file path. Shared with the prompt-
/// composition layer so the agent receives the same absolute path
/// it must record against.
#[must_use]
pub(crate) fn closeout_attest_path(state_root: &std::path::Path, pr: PullRequestNumber) -> PathBuf {
    state_root.join(pr.to_string()).join(CLOSEOUT_FILE)
}

/// Read the attestation against the current HEAD.
pub(crate) fn observe_closeout(
    state_root: Option<&std::path::Path>,
    pr: PullRequestNumber,
    head_sha: &GitCommitSha,
) -> CloseoutObservation {
    let path = state_root.map(|root| closeout_attest_path(root, pr));
    let attestation = path
        .as_deref()
        .and_then(|p| read_closeout(p).ok().flatten());
    CloseoutObservation {
        attestation,
        head_sha: head_sha.clone(),
        attest_path: path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ooda_core::attest::{CLOSEOUT_SCHEMA_VERSION, write_closeout_atomic};
    use tempfile::tempdir;

    const VALID_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(VALID_SHA).unwrap()
    }

    #[test]
    fn attest_path_joins_pull_request_id_and_filename() {
        let p = closeout_attest_path(std::path::Path::new("/state"), pr());
        assert_eq!(
            p,
            std::path::PathBuf::from("/state/753/closeout_attest.json")
        );
    }

    #[test]
    fn missing_state_root_yields_no_attestation() {
        let obs = observe_closeout(None, pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.attest_path.is_none());
        assert_eq!(obs.head_sha, head());
    }

    #[test]
    fn missing_attestation_file_yields_none() {
        let dir = tempdir().unwrap();
        let obs = observe_closeout(Some(dir.path()), pr(), &head());
        assert!(obs.attestation.is_none());
    }

    #[test]
    fn attestation_matching_head_round_trips() {
        let dir = tempdir().unwrap();
        let path = closeout_attest_path(dir.path(), pr());
        write_closeout_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs = observe_closeout(Some(dir.path()), pr(), &head());
        let att = obs.attestation.expect("attestation should be present");
        assert_eq!(att.attested_sha, VALID_SHA);
        assert_eq!(att.version, CLOSEOUT_SCHEMA_VERSION);
    }

    #[test]
    fn malformed_attestation_file_degrades_to_none() {
        let dir = tempdir().unwrap();
        let path = closeout_attest_path(dir.path(), pr());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let obs = observe_closeout(Some(dir.path()), pr(), &head());
        assert!(obs.attestation.is_none());
    }

    #[test]
    fn round_trip_observation_round_trips_attested_sha() {
        let dir = tempdir().unwrap();
        let path = closeout_attest_path(dir.path(), pr());
        let written = write_closeout_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs = observe_closeout(Some(dir.path()), pr(), &head());
        assert_eq!(obs.attestation.as_ref().unwrap(), &written);
    }

    #[test]
    fn closeout_observation_serializes_with_typed_fields() {
        let obs = CloseoutObservation {
            attestation: Some(CloseoutAttestation {
                attested_sha: VALID_SHA.to_string(),
                attested_at: Utc::now(),
                version: CLOSEOUT_SCHEMA_VERSION,
            }),
            head_sha: GitCommitSha::parse(OTHER_SHA).unwrap(),
            attest_path: Some(std::path::PathBuf::from("/state/753/closeout_attest.json")),
        };
        let json = serde_json::to_string(&obs).unwrap();
        assert!(json.contains(VALID_SHA));
        assert!(json.contains(OTHER_SHA));
        assert!(json.contains("/state/753/closeout_attest.json"));
    }

    #[test]
    fn observation_attest_path_present_when_state_root_supplied() {
        let dir = tempdir().unwrap();
        let obs = observe_closeout(Some(dir.path()), pr(), &head());
        let path = obs.attest_path.expect("path should be present");
        assert!(path.ends_with("753/closeout_attest.json"));
    }
}
