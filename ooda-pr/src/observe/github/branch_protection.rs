//! Legacy-source projection for branch-level protection.
//!
//! Fetches the parent `branches/{branch}/protection` endpoint so a
//! single observation covers required-status-checks, conversation-
//! resolution, signed-commits, and other policy bits that gate merge.
//!
//! # Invariants
//!
//! - **Absence is typed**: unconfigured branches lift the 404 into
//!   `Ok(None)`. Transport errors propagate.
//! - **Sub-field absence is structural**: every gate exposes its
//!   own `Option`; an unset policy reads as `None`, distinct from
//!   `Some(false)` (configured-and-off).

use serde::{Deserialize, Serialize};

use crate::ids::{CheckName, RepoSlug};

use super::gh::{GhError, encode_path_segment, gh_json};

/// Fetch the full branch-protection object for a branch. Absence
/// (unconfigured / branch not protected) lifts to `Ok(None)`;
/// transport errors propagate. `branch` is path-segment-encoded.
pub(crate) fn fetch_branch_protection(
    slug: &RepoSlug,
    branch: &str,
) -> Result<Option<BranchProtection>, GhError> {
    let path = format!(
        "repos/{slug}/branches/{}/protection",
        encode_path_segment(branch)
    );
    match gh_json::<BranchProtection>(&["api", &path]) {
        Ok(v) => Ok(Some(v)),
        Err(GhError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Full branch-protection projection. Every sub-policy is optional
/// because the host omits the field entirely when the policy is not
/// configured.
///
/// Field names mirror the GitHub wire shape verbatim — the shared
/// `required_` prefix is the host's vocabulary, not redundant
/// naming, so the boundary mapping stays unambiguous.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub(crate) struct BranchProtection {
    #[serde(default)]
    pub required_status_checks: Option<BranchProtectionRequiredStatusChecks>,
    #[serde(default)]
    pub required_conversation_resolution: Option<EnabledFlag>,
    #[serde(default)]
    pub required_signatures: Option<EnabledFlag>,
}

impl BranchProtection {
    /// `true` iff conversation-resolution is configured-and-on.
    /// `None` and `Some(false)` both read as off.
    pub(crate) fn conversation_resolution_enabled(&self) -> bool {
        self.required_conversation_resolution
            .as_ref()
            .is_some_and(|f| f.enabled)
    }

    /// `true` iff signed-commits is configured-and-on.
    pub(crate) fn signatures_enabled(&self) -> bool {
        self.required_signatures.as_ref().is_some_and(|f| f.enabled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct EnabledFlag {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct BranchProtectionRequiredStatusChecks {
    #[serde(default)]
    pub checks: Vec<BranchProtectionCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct BranchProtectionCheck {
    pub context: CheckName,
    /// Absent when the check is not pinned to a registered host app
    /// (status posted without app integration).
    pub app_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../test/fixtures/github/branch_protection.json");

    #[test]
    fn deserializes_full_fixture() {
        let resp: BranchProtection = serde_json::from_str(FIXTURE).unwrap();
        let checks = resp
            .required_status_checks
            .as_ref()
            .expect("fixture has required_status_checks");
        assert_eq!(checks.checks.len(), 2);
        assert_eq!(
            checks.checks[0].context.as_str(),
            "Graphite / mergeability_check"
        );
        assert_eq!(checks.checks[0].app_id, Some(158_384));
        assert!(resp.conversation_resolution_enabled());
        assert!(resp.signatures_enabled());
    }

    #[test]
    fn absent_subfields_decode_to_none() {
        let json = r#"{"url":"https://x"}"#;
        let resp: BranchProtection = serde_json::from_str(json).unwrap();
        assert!(resp.required_status_checks.is_none());
        assert!(resp.required_conversation_resolution.is_none());
        assert!(resp.required_signatures.is_none());
        assert!(!resp.conversation_resolution_enabled());
        assert!(!resp.signatures_enabled());
    }

    #[test]
    fn explicit_disabled_reads_as_off() {
        let json = r#"{
            "required_conversation_resolution": {"enabled": false},
            "required_signatures": {"enabled": false}
        }"#;
        let resp: BranchProtection = serde_json::from_str(json).unwrap();
        assert!(!resp.conversation_resolution_enabled());
        assert!(!resp.signatures_enabled());
    }

    #[test]
    fn null_app_id_tolerated() {
        let json =
            r#"{"required_status_checks":{"checks":[{"context":"Custom Status","app_id":null}]}}"#;
        let resp: BranchProtection = serde_json::from_str(json).unwrap();
        let checks = resp.required_status_checks.unwrap();
        assert_eq!(checks.checks[0].app_id, None);
    }

    #[test]
    fn missing_checks_field_in_required_status_checks_defaults_to_empty() {
        let json = r#"{"required_status_checks":{"url":"https://x"}}"#;
        let resp: BranchProtection = serde_json::from_str(json).unwrap();
        let checks = resp.required_status_checks.unwrap();
        assert!(checks.checks.is_empty());
    }
}
