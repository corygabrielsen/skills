//! Legacy-source projection for branch-level required checks.
//!
//! Distinct from the rule-source projection; either source alone
//! understates the required-check set on repos that use both. A
//! not-found response means "unconfigured" rather than "absent
//! resource" — the wrapper lifts it to typed absence.

use serde::{Deserialize, Serialize};

use crate::ids::{CheckName, RepoSlug};

use super::gh::{GhError, encode_path_segment, gh_json};

/// Fetch the legacy-source required-check list for a branch.
/// Absence (unconfigured) lifts to `Ok(None)`; transport errors
/// propagate. `branch` is path-segment-encoded.
pub(crate) fn fetch_branch_protection_required_checks(
    slug: &RepoSlug,
    branch: &str,
) -> Result<Option<BranchProtectionRequiredStatusChecks>, GhError> {
    let path = format!(
        "repos/{slug}/branches/{}/protection/required_status_checks",
        encode_path_segment(branch)
    );
    match gh_json::<BranchProtectionRequiredStatusChecks>(&["api", &path]) {
        Ok(v) => Ok(Some(v)),
        Err(GhError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
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

    const FIXTURE: &str =
        include_str!("../../../test/fixtures/github/branch_protection_required_checks.json");

    #[test]
    fn deserializes_fixture() {
        let resp: BranchProtectionRequiredStatusChecks = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(resp.checks.len(), 2);
        assert_eq!(
            resp.checks[0].context.as_str(),
            "Graphite / mergeability_check"
        );
        assert_eq!(resp.checks[0].app_id, Some(158_384));
        assert_eq!(resp.checks[1].context.as_str(), "Mergeability Check");
    }

    #[test]
    fn null_app_id_tolerated() {
        let json = r#"{"checks":[{"context":"Custom Status","app_id":null}]}"#;
        let resp: BranchProtectionRequiredStatusChecks = serde_json::from_str(json).unwrap();
        assert_eq!(resp.checks[0].app_id, None);
    }

    #[test]
    fn missing_checks_field_defaults_to_empty() {
        let json = r#"{"url":"https://x"}"#;
        let resp: BranchProtectionRequiredStatusChecks = serde_json::from_str(json).unwrap();
        assert!(resp.checks.is_empty());
    }
}
