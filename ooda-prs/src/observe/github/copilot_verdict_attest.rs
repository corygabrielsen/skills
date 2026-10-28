//! Observation for the reviewer-verdict attestation axis.
//!
//! Same protocol shape as the sibling attestation observations:
//! attestation read, malformed-file degradation to absence. Id-keyed:
//! orient joins each entry to the review it names, so neither HEAD
//! nor the clock carries signal here.

use std::path::PathBuf;

use ooda_core::attest::{CopilotVerdictAttestation, read_copilot_verdict};
use serde::Serialize;

use crate::ids::PullRequestNumber;

const COPILOT_VERDICT_FILE: &str = "copilot_verdict_attest.json";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CopilotVerdictObservation {
    pub attestation: Option<CopilotVerdictAttestation>,
    pub attest_path: Option<PathBuf>,
}

/// Compose the attestation file path. Shared with the prompt-
/// composition layer so the agent receives the same absolute path
/// it must record against.
#[must_use]
pub(crate) fn copilot_verdict_attest_path(
    state_root: &std::path::Path,
    pr: PullRequestNumber,
) -> PathBuf {
    state_root.join(pr.to_string()).join(COPILOT_VERDICT_FILE)
}

/// Read the attestation. Absent state-root degrades to "no
/// attestation possible" without touching the filesystem.
pub(crate) fn observe_copilot_verdict(
    state_root: Option<&std::path::Path>,
    pr: PullRequestNumber,
) -> CopilotVerdictObservation {
    let path = state_root.map(|root| copilot_verdict_attest_path(root, pr));
    let attestation = path
        .as_deref()
        .and_then(|p| read_copilot_verdict(p).ok().flatten());
    CopilotVerdictObservation {
        attestation,
        attest_path: path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooda_core::attest::{VerdictDisposition, upsert_copilot_verdict_atomic};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    #[test]
    fn path_joins_state_root_pr_and_file() {
        let p = copilot_verdict_attest_path(std::path::Path::new("/state"), pr());
        assert_eq!(p, PathBuf::from("/state/753/copilot_verdict_attest.json"));
    }

    #[test]
    fn absent_state_root_yields_no_path_and_no_attestation() {
        let obs = observe_copilot_verdict(None, pr());
        assert!(obs.attest_path.is_none());
        assert!(obs.attestation.is_none());
    }

    #[test]
    fn missing_file_yields_path_without_attestation() {
        let dir = tempfile::tempdir().unwrap();
        let obs = observe_copilot_verdict(Some(dir.path()), pr());
        assert!(obs.attest_path.is_some());
        assert!(obs.attestation.is_none());
    }

    #[test]
    fn malformed_file_degrades_to_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = copilot_verdict_attest_path(dir.path(), pr());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(
            observe_copilot_verdict(Some(dir.path()), pr())
                .attestation
                .is_none()
        );
    }

    #[test]
    fn written_entry_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = copilot_verdict_attest_path(dir.path(), pr());
        upsert_copilot_verdict_atomic(
            &path,
            SHA.to_string(),
            42,
            VerdictDisposition::Escalate,
            "needs a human".to_string(),
        )
        .unwrap();
        let att = observe_copilot_verdict(Some(dir.path()), pr())
            .attestation
            .unwrap();
        assert_eq!(
            att.entry(42).unwrap().disposition,
            VerdictDisposition::Escalate
        );
    }
}
