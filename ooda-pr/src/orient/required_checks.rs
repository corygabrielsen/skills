//! Union of the two authoritative required-check sources:
//!   - `rules/branches/{branch}` (modern rulesets)
//!   - `branches/{branch}/protection/required_status_checks` (legacy)
//!
//! Either source alone is incomplete; repos may use one, the other,
//! or both. The output is the deduped union of context names.

use crate::ids::CheckName;
use crate::observe::github::branch_protection::BranchProtectionRequiredStatusChecks;
use crate::observe::github::branch_rules::BranchRule;
use crate::observe::github::rulesets::RequiredStatusChecksParams;

/// Resolve the full set of required check names from rulesets +
/// legacy branch protection. Order is `rulesets-first, then
/// protection`; duplicates dropped on second occurrence.
///
/// Known limitation — duplicate-context different-app:
/// dedup is by `context` name only, not by `(context,
/// integration_id)`. A repo configuring two required checks with
/// the same name from different GitHub Apps (e.g. two CI
/// providers each posting "Lint") collapses to one required entry,
/// and a single passing check appears to satisfy both. End-to-end
/// fix requires `gh pr checks` to surface `app_id` (it does not),
/// so observed checks can be matched per-app. Until the source
/// data carries integration identity, this configuration produces
/// a green report when GitHub still blocks merge. Surface as a
/// known false-green; the merge-state fallback (BLOCKED handoff)
/// catches it via `mergeStateStatus` when no other axis explains
/// the blockage.
pub fn required_check_names(
    branch_rules: &[BranchRule],
    protection: Option<&BranchProtectionRequiredStatusChecks>,
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

    if let Some(p) = protection {
        for c in &p.checks {
            push_unique(c.context.clone(), &mut out);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::branch_protection::{
        BranchProtectionCheck, BranchProtectionRequiredStatusChecks,
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

    fn protection(contexts: &[&str]) -> BranchProtectionRequiredStatusChecks {
        BranchProtectionRequiredStatusChecks {
            checks: contexts
                .iter()
                .map(|c| BranchProtectionCheck {
                    context: CheckName::parse(c).unwrap(),
                    app_id: None,
                })
                .collect(),
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
