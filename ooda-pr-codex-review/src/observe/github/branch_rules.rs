//! Rule-source projection for branch-level rules.
//!
//! Yields every active rule targeting the given branch, across all
//! configured rule sources. Rule parameters are polymorphic — each
//! row carries the parameter payload as opaque JSON; typed
//! parameter projections deserialize on demand.

use serde::{Deserialize, Serialize};

use crate::ids::RepoSlug;

use super::gh::{GhError, encode_path_segment, gh_json_paginate};

/// Fetch every active rule targeting `branch`. Branch name is path-
/// segment-encoded; pagination is fetcher-side because dropping
/// later pages silently removes required-check rules from the
/// decision model and would make a still-blocked PR look clean.
pub(crate) fn fetch_branch_rules(
    slug: &RepoSlug,
    branch: &str,
) -> Result<Vec<BranchRule>, GhError> {
    let path = format!(
        "repos/{slug}/rules/branches/{}?per_page=100",
        encode_path_segment(branch)
    );
    gh_json_paginate(&["api", "--paginate", &path])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct BranchRule {
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
        assert_eq!(copilot.ruleset_id, 12_592_595);
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
