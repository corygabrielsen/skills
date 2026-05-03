//! Typed view of
//! `GET /repos/{o}/{r}/branches/{branch}/protection/required_status_checks`.
//!
//! Legacy branch-protection source for required checks (distinct
//! from rulesets). Returns `404` when no classic protection is
//! configured; callers handle that at the observer layer.

use serde::Deserialize;

use crate::ids::{CheckName, RepoSlug};

use super::gh::{encode_path_segment, gh_json, GhError};

/// Fetch legacy branch-protection required status checks. Returns
/// `Ok(None)` when the endpoint returns 404 (no classic protection
/// configured — a normal, non-error state). `branch` is URL-encoded
/// so names with `/` resolve to one path segment.
pub fn fetch_branch_protection_required_checks(
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BranchProtectionRequiredStatusChecks {
    #[serde(default)]
    pub checks: Vec<BranchProtectionCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BranchProtectionCheck {
    pub context: CheckName,
    /// Null when the check has no registered GitHub App (e.g. status
    /// posted without app integration).
    pub app_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!(
        "../../../test/fixtures/github/branch_protection_required_checks.json"
    );

    #[test]
    fn deserializes_fixture() {
        let resp: BranchProtectionRequiredStatusChecks =
            serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(resp.checks.len(), 2);
        assert_eq!(resp.checks[0].context.as_str(), "Graphite / mergeability_check");
        assert_eq!(resp.checks[0].app_id, Some(158384));
        assert_eq!(resp.checks[1].context.as_str(), "Mergeability Check");
    }

    #[test]
    fn null_app_id_tolerated() {
        let json = r#"{"checks":[{"context":"Custom Status","app_id":null}]}"#;
        let resp: BranchProtectionRequiredStatusChecks =
            serde_json::from_str(json).unwrap();
        assert_eq!(resp.checks[0].app_id, None);
    }

    #[test]
    fn missing_checks_field_defaults_to_empty() {
        let json = r#"{"url":"https://x"}"#;
        let resp: BranchProtectionRequiredStatusChecks =
            serde_json::from_str(json).unwrap();
        assert!(resp.checks.is_empty());
    }
}
