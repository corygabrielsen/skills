//! Resolve the gating check-context set for a branch.
//!
//! # Invariants
//!
//! - **Source-completeness**: a repo may declare required checks via
//!   any combination of the rule-source and the legacy-protection-
//!   source. Either source read alone may understate the gate; the
//!   resolved set is their union.
//! - **Context-name as identity**: dedup partitions by check-context
//!   name, not by `(name, app)`. The known false-green this admits
//!   (same context name posted by two distinct apps) is documented
//!   on the function and caught downstream by the merge-state
//!   fallback.
//! - **Order is source-precedence**: rule-sourced contexts precede
//!   legacy-sourced contexts; within each source, input order is
//!   preserved. Stable for human-readable rendering.

use crate::ids::CheckName;
use crate::observe::github::branch_protection::BranchProtection;
use crate::observe::github::branch_rules::BranchRule;
use crate::observe::github::rulesets::RequiredStatusChecksParams;

/// Resolved required-check set as the union of the rule source and
/// the legacy-protection source, deduped by context name.
///
/// **Known false-green**: a repo configuring the same context name
/// from two distinct GitHub Apps collapses to one entry, and a
/// single passing check appears to satisfy both gates. Source data
/// must carry integration identity to fix end-to-end. The
/// merge-state fallback catches the residual case when no axis
/// explains the merge block.
pub(crate) fn required_check_names(
    branch_rules: &[BranchRule],
    protection: Option<&BranchProtection>,
) -> Vec<CheckName> {
    let mut seen = std::collections::HashSet::<String>::new();
    let mut out: Vec<CheckName> = Vec::new();

    let mut push_unique = |c: CheckName, out: &mut Vec<CheckName>| {
        if seen.insert(c.as_str().to_owned()) {
            out.push(c);
        }
    };

    for rule in branch_rules {
        if rule.rule_type != "required_status_checks" {
            continue;
        }
        let Some(params) = rule.parameters.clone() else {
            continue;
        };
        let Ok(parsed): Result<RequiredStatusChecksParams, _> = serde_json::from_value(params)
        else {
            continue;
        };
        for c in parsed.required_status_checks {
            push_unique(c.context, &mut out);
        }
    }

    if let Some(checks) = protection.and_then(|p| p.required_status_checks.as_ref()) {
        for c in &checks.checks {
            push_unique(c.context.clone(), &mut out);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::branch_protection::{
        BranchProtection, BranchProtectionCheck, BranchProtectionRequiredStatusChecks,
    };

    fn rule(rule_type: &str, contexts: &[(&str, u64)]) -> BranchRule {
        let params = serde_json::json!({
            "required_status_checks": contexts.iter().map(|(c, id)| serde_json::json!({
                "context": c,
                "integration_id": id,
            })).collect::<Vec<_>>(),
        });
        BranchRule {
            rule_type: rule_type.into(),
            parameters: Some(params),
            ruleset_id: 1,
            ruleset_source: "x/y".into(),
            ruleset_source_type: "Repository".into(),
        }
    }

    fn rule_no_params(rule_type: &str) -> BranchRule {
        BranchRule {
            rule_type: rule_type.into(),
            parameters: None,
            ruleset_id: 1,
            ruleset_source: "x/y".into(),
            ruleset_source_type: "Repository".into(),
        }
    }

    fn protection(contexts: &[&str]) -> BranchProtection {
        BranchProtection {
            required_status_checks: Some(BranchProtectionRequiredStatusChecks {
                checks: contexts
                    .iter()
                    .map(|c| BranchProtectionCheck {
                        context: CheckName::parse(c).unwrap(),
                        app_id: None,
                    })
                    .collect(),
            }),
            required_conversation_resolution: None,
            required_signatures: None,
        }
    }

    fn names(checks: Vec<CheckName>) -> Vec<String> {
        checks.into_iter().map(|c| c.as_str().to_owned()).collect()
    }

    #[test]
    fn empty_inputs_yield_empty_output() {
        assert!(required_check_names(&[], None).is_empty());
    }

    #[test]
    fn rulesets_only() {
        let rules = vec![rule("required_status_checks", &[("Lint", 1), ("Build", 1)])];
        assert_eq!(
            names(required_check_names(&rules, None)),
            vec!["Lint", "Build"],
        );
    }

    #[test]
    fn protection_only() {
        let p = protection(&["Mergeability Check", "Lint"]);
        assert_eq!(
            names(required_check_names(&[], Some(&p))),
            vec!["Mergeability Check", "Lint"],
        );
    }

    #[test]
    fn union_dedupes_overlap() {
        let rules = vec![rule("required_status_checks", &[("Lint", 1), ("Build", 1)])];
        let p = protection(&["Lint", "Mergeability Check"]); // Lint dup
        assert_eq!(
            names(required_check_names(&rules, Some(&p))),
            vec!["Lint", "Build", "Mergeability Check"],
        );
    }

    #[test]
    fn non_required_rule_types_ignored() {
        let rules = vec![
            rule_no_params("creation"),
            rule_no_params("update"),
            rule("required_status_checks", &[("Lint", 1)]),
        ];
        assert_eq!(names(required_check_names(&rules, None)), vec!["Lint"]);
    }

    #[test]
    fn rule_with_unparseable_parameters_skipped() {
        let mut bad = rule("required_status_checks", &[("Lint", 1)]);
        bad.parameters = Some(serde_json::json!({"unexpected": "shape"}));
        assert!(required_check_names(&[bad], None).is_empty());
    }
}
