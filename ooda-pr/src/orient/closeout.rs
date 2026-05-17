//! Final sign-off axis. Pure projection of an attestation observation.
//!
//! # Invariants
//!
//! - **Binary drift**: drift is the SHA-inequality bit alone — no
//!   distance metric. A one-commit gap and a hundred-commit gap are
//!   the same state, because the handoff is a fresh full read of the
//!   PR, not an incremental review of what changed since the last
//!   attestation.
//! - **Absence ≠ drift**: a never-recorded attestation is its own
//!   variant. Collapsing it into drift would lose the "no human has
//!   ever signed off" signal the prompt depends on.
//! - **Pure projection**: orient does not consult the clock, the
//!   network, or any other observation. Same input → same output.

use serde::Serialize;

use crate::observe::github::closeout_attest::CloseoutObservation;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum Closeout {
    Synced,
    Drift {
        attested_sha: String,
        head_sha: String,
    },
    NeverAttested,
}

/// Project a `CloseoutObservation` into the typed axis.
#[must_use]
pub(crate) fn orient_closeout(obs: &CloseoutObservation) -> Closeout {
    match &obs.attestation {
        None => Closeout::NeverAttested,
        Some(att) if att.attested_sha == obs.head_sha.as_str() => Closeout::Synced,
        Some(att) => Closeout::Drift {
            attested_sha: att.attested_sha.clone(),
            head_sha: obs.head_sha.as_str().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::GitCommitSha;
    use chrono::Utc;
    use ooda_core::attest::{CLOSEOUT_SCHEMA_VERSION, CloseoutAttestation};

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn attestation(sha: &str) -> CloseoutAttestation {
        CloseoutAttestation {
            attested_sha: sha.to_string(),
            attested_at: Utc::now(),
            version: CLOSEOUT_SCHEMA_VERSION,
        }
    }

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
    }

    #[test]
    fn no_attestation_yields_never_attested() {
        let obs = CloseoutObservation {
            attestation: None,
            head_sha: head(),
            attest_path: None,
        };
        assert_eq!(orient_closeout(&obs), Closeout::NeverAttested);
    }

    #[test]
    fn matching_sha_yields_synced() {
        let obs = CloseoutObservation {
            attestation: Some(attestation(HEAD_SHA)),
            head_sha: head(),
            attest_path: None,
        };
        assert_eq!(orient_closeout(&obs), Closeout::Synced);
    }

    #[test]
    fn mismatched_sha_yields_drift() {
        let obs = CloseoutObservation {
            attestation: Some(attestation(OTHER_SHA)),
            head_sha: head(),
            attest_path: None,
        };
        assert_eq!(
            orient_closeout(&obs),
            Closeout::Drift {
                attested_sha: OTHER_SHA.to_string(),
                head_sha: HEAD_SHA.to_string(),
            }
        );
    }

    #[test]
    fn round_trip_attestation_classifies_as_synced() {
        use crate::ids::PullRequestNumber;
        use crate::observe::github::closeout_attest::{closeout_attest_path, observe_closeout};
        use ooda_core::attest::write_closeout_atomic;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pr = PullRequestNumber::parse("753").unwrap();
        let head_sha = GitCommitSha::parse(HEAD_SHA).unwrap();

        write_closeout_atomic(&closeout_attest_path(dir.path(), pr), HEAD_SHA.to_string()).unwrap();

        let obs = observe_closeout(Some(dir.path()), pr, &head_sha);
        assert_eq!(orient_closeout(&obs), Closeout::Synced);
    }
}
