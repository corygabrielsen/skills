//! Observe the PR-meta attestation file plus its drift against
//! the current PR head.
//!
//! Read flow:
//!   1. Read `<state-root>/<pr-id>/pr_meta_attest.json` (may be
//!      absent — a never-attested PR is a valid steady state).
//!   2. If attested SHA differs from HEAD, query `gh api compare`
//!      for the count of commits between the two. A failed compare
//!      (e.g. attested SHA pruned post-rebase) preserves
//!      `commits_behind = None`; the orient layer projects Drift
//!      regardless (the SHA mismatch is the trigger, not the
//!      count) but distinguishes "drift exists, count unknown"
//!      from "drift exists, N commits behind."
//!
//! The attestation read NEVER fails the observe pass. Malformed
//! files, version mismatches, and bad SHAs all collapse to
//! `attestation = None`; the orient layer then projects
//! `NeverAttested` and decide hands off to the agent to re-attest.

use std::path::PathBuf;

use ooda_core::attest::{PullRequestMetadataAttestation, read_pull_request_metadata};
use serde::Serialize;

use crate::ids::{GitCommitSha, PullRequestNumber, RepoSlug};

use super::gh::{GhError, encode_path_segment, gh_json};

const PULL_REQUEST_METADATA_FILE: &str = "pr_meta_attest.json";

/// Observation triple consumed by the orient layer.
///
/// `attest_path` is the absolute path the agent must pass to
/// `ooda-attest pr-meta --state-root <root>` after refreshing PR
/// metadata. `None` when no `--state-root` was supplied — the act
/// layer falls back to a path-free prompt that asks the agent to
/// supply the state-root themselves.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PullRequestMetadataObservation {
    pub attestation: Option<PullRequestMetadataAttestation>,
    pub head_sha: GitCommitSha,
    pub commits_behind: Option<usize>,
    pub attest_path: Option<PathBuf>,
}

/// Compose the attestation file path the CLI (`ooda-attest pr-meta`)
/// writes to. Pulled out so the act-layer prompt composer can
/// surface the same absolute path the agent must pass to the CLI.
#[must_use]
pub(crate) fn attest_path(state_root: &std::path::Path, pr: PullRequestNumber) -> PathBuf {
    state_root
        .join(pr.to_string())
        .join(PULL_REQUEST_METADATA_FILE)
}

/// Observe the PR-meta attestation + drift against `head_sha`.
///
/// `state_root` is `None` when the caller omitted `--state-root`;
/// the observation degrades to "no attestation file possible" —
/// orient classifies as `NeverAttested` and the dashboard surfaces
/// the gap. No filesystem touch in that case.
pub(crate) fn observe_pull_request_metadata(
    state_root: Option<&std::path::Path>,
    slug: &RepoSlug,
    pr: PullRequestNumber,
    head_sha: &GitCommitSha,
) -> PullRequestMetadataObservation {
    let path = state_root.map(|root| attest_path(root, pr));
    let attestation = path
        .as_deref()
        .and_then(|p| read_pull_request_metadata(p).ok().flatten());
    let commits_behind = match &attestation {
        Some(att) if att.attested_sha != head_sha.as_str() => {
            compare_ahead_by(slug, &att.attested_sha, head_sha)
        }
        _ => None,
    };
    PullRequestMetadataObservation {
        attestation,
        head_sha: head_sha.clone(),
        commits_behind,
        attest_path: path,
    }
}

/// Query `gh api compare` for the `ahead_by` count between
/// `attested_sha` and `head`. The compare endpoint returns
/// `behind_by` (commits on base since merge base) and `ahead_by`
/// (commits on head since merge base); we report `ahead_by` —
/// commits the PR has added since the attestation was recorded.
///
/// `None` when the compare fetch fails for any reason (e.g.
/// attested SHA pruned post-rebase, HTTP 404, transport error).
/// The caller treats `None` as "drift exists but count unknown."
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
    use ooda_core::attest::{
        PULL_REQUEST_METADATA_SCHEMA_VERSION, write_pull_request_metadata_atomic,
    };
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
        let p = attest_path(std::path::Path::new("/state"), pr());
        assert_eq!(
            p,
            std::path::PathBuf::from("/state/753/pr_meta_attest.json")
        );
    }

    #[test]
    fn missing_state_root_yields_no_attestation_and_no_compare() {
        let obs = observe_pull_request_metadata(None, &slug(), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.commits_behind.is_none());
        assert_eq!(obs.head_sha, head());
    }

    #[test]
    fn missing_attestation_file_yields_none_without_touching_gh() {
        let dir = tempdir().unwrap();
        let obs = observe_pull_request_metadata(Some(dir.path()), &slug(), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.commits_behind.is_none());
    }

    #[test]
    fn attestation_matching_head_yields_no_commits_behind_query() {
        let dir = tempdir().unwrap();
        let path = attest_path(dir.path(), pr());
        write_pull_request_metadata_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs = observe_pull_request_metadata(Some(dir.path()), &slug(), pr(), &head());
        let att = obs.attestation.expect("attestation should be present");
        assert_eq!(att.attested_sha, VALID_SHA);
        assert_eq!(att.version, PULL_REQUEST_METADATA_SCHEMA_VERSION);
        assert!(obs.commits_behind.is_none());
    }

    #[test]
    fn malformed_attestation_file_degrades_to_none() {
        let dir = tempdir().unwrap();
        let path = attest_path(dir.path(), pr());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let obs = observe_pull_request_metadata(Some(dir.path()), &slug(), pr(), &head());
        assert!(obs.attestation.is_none());
        assert!(obs.commits_behind.is_none());
    }

    #[test]
    fn round_trip_observation_round_trips_attested_sha() {
        let dir = tempdir().unwrap();
        let path = attest_path(dir.path(), pr());
        let written = write_pull_request_metadata_atomic(&path, VALID_SHA.to_string()).unwrap();
        let obs = observe_pull_request_metadata(Some(dir.path()), &slug(), pr(), &head());
        assert_eq!(obs.attestation.as_ref().unwrap(), &written);
    }

    #[test]
    fn pull_request_metadata_observation_serializes_with_typed_fields() {
        let obs = PullRequestMetadataObservation {
            attestation: Some(PullRequestMetadataAttestation {
                attested_sha: VALID_SHA.to_string(),
                attested_at: Utc::now(),
                version: PULL_REQUEST_METADATA_SCHEMA_VERSION,
            }),
            head_sha: GitCommitSha::parse(OTHER_SHA).unwrap(),
            commits_behind: Some(3),
            attest_path: Some(std::path::PathBuf::from("/state/753/pr_meta_attest.json")),
        };
        let json = serde_json::to_string(&obs).unwrap();
        assert!(json.contains(VALID_SHA));
        assert!(json.contains(OTHER_SHA));
        assert!(json.contains("\"commits_behind\":3"));
        assert!(json.contains("/state/753/pr_meta_attest.json"));
    }

    #[test]
    fn observation_attest_path_present_when_state_root_supplied() {
        let dir = tempdir().unwrap();
        let obs = observe_pull_request_metadata(Some(dir.path()), &slug(), pr(), &head());
        let path = obs.attest_path.expect("path should be present");
        assert!(path.ends_with("753/pr_meta_attest.json"));
    }

    #[test]
    fn observation_attest_path_absent_when_state_root_missing() {
        let obs = observe_pull_request_metadata(None, &slug(), pr(), &head());
        assert!(obs.attest_path.is_none());
    }
}
