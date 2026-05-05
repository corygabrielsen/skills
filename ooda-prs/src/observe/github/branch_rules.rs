//! Typed view of `GET /repos/{o}/{r}/rules/branches/{branch}`.
//!
//! Returns all active rules (across all rulesets) that target the
//! given branch. Each entry carries both the rule body (type +
//! parameters) and the ruleset metadata identifying which ruleset
//! produced it. Rule parameters are polymorphic — see
//! [`crate::observe::github::rulesets`] for typed parameter structs.

use serde::{Deserialize, Serialize};

use crate::ids::RepoSlug;

use super::gh::{GhError, encode_path_segment, gh_json_paginate};

/// Fetch all active rules (across all rulesets) that target a
/// specific branch via `GET /repos/{o}/{r}/rules/branches/{branch}`.
/// `branch` is URL-encoded — branches like `release/1.2` would
/// otherwise be treated as multiple path segments.
///
/// Uses `--paginate` with `per_page=100`. The endpoint returns at
/// most 30 rules per page by default; on branches with many active
/// rulesets, later pages would be silently dropped without
/// pagination, causing required-status-checks rules to disappear
/// from the decision model and a still-blocked PR to look clean.
/// Class invariant: every list endpoint that may exceed one page
/// uses `gh_json_paginate`.
pub fn fetch_branch_rules(slug: &RepoSlug, branch: &str) -> Result<Vec<BranchRule>, GhError> {
    let path = format!(
        "repos/{slug}/rules/branches/{}?per_page=100",
        encode_path_segment(branch)
    );
    gh_json_paginate(&["api", "--paginate", &path])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BranchRule {
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    pub ruleset_id: u64,
    pub ruleset_source: String,
    pub ruleset_source_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../test/fixtures/github/branch_rules_master.json");

    #[test]
    fn deserializes_fixture() {
        let rules: Vec<BranchRule> = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(rules.len(), 2);

        let commit_msg = &rules[0];
        assert_eq!(commit_msg.rule_type, "commit_message_pattern");
        assert!(commit_msg.parameters.is_some());
        assert_eq!(commit_msg.ruleset_source, "acme/protocol");
        assert_eq!(commit_msg.ruleset_source_type, "Repository");

        let copilot = &rules[1];
        assert_eq!(copilot.rule_type, "copilot_code_review");
        assert_eq!(copilot.ruleset_id, 12592595);
    }

    #[test]
    fn required_status_checks_params_extractable() {
        use super::super::rulesets::RequiredStatusChecksParams;
        let json = r#"[{
            "type": "required_status_checks",
            "parameters": {
                "required_status_checks": [
                    {"context": "Lint", "integration_id": 15368}
                ]
            },
            "ruleset_source_type": "Repository",
            "ruleset_source": "acme/protocol",
            "ruleset_id": 1
        }]"#;
        let rules: Vec<BranchRule> = serde_json::from_str(json).unwrap();
        let params: RequiredStatusChecksParams =
            serde_json::from_value(rules[0].parameters.clone().unwrap()).unwrap();
        assert_eq!(params.required_status_checks.len(), 1);
        assert_eq!(params.required_status_checks[0].context.as_str(), "Lint");
    }

    #[test]
    fn unknown_rule_type_tolerated() {
        let json = r#"[{
            "type": "future_rule",
            "parameters": null,
            "ruleset_source_type": "Repository",
            "ruleset_source": "acme/protocol",
            "ruleset_id": 999
        }]"#;
        let rules: Vec<BranchRule> = serde_json::from_str(json).unwrap();
        assert_eq!(rules[0].rule_type, "future_rule");
        assert_eq!(rules[0].parameters, None);
    }
}
