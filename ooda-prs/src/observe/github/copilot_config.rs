//! Resolve the reviewer-axis configuration applicable to a branch.
//!
//! Walks the rule-source list in parallel and returns the first
//! qualifying configuration. "Qualifying" requires both that the
//! source be active and that its branch-scoping admit the target
//! branch — see invariants below.
//!
//! # Invariants
//!
//! - **Active + admitting**: a rule source contributes only when
//!   its enforcement state is active and its scoping conditions
//>   admit the target branch. Earlier code that ignored the second
//!   half misreported configuration on branches outside the rule's
//!   scope.
//! - **Missing parameters means defaults**: an active rule with no
//!   parameters payload uses host-default values; ignoring such
//!   rules would misreport repos that rely on defaults as
//!   unconfigured.

use std::thread;

use crate::ids::RepoSlug;

use super::gh::GhError;
use super::rulesets::{
    CopilotCodeReviewParams, Ruleset, RulesetEnforcement, fetch_ruleset, fetch_ruleset_list,
    ruleset_matches_branch,
};

/// Resolve the reviewer-axis configuration for `branch`.
///
/// Returns absence when no qualifying rule source exists; error
/// only on list-level failure or non-not-found detail failure.
/// `branch` must be the resolved stack-root branch — passing an
/// intermediate branch would misreport configuration on every
/// stacked PR.
pub(crate) fn fetch_copilot_config(
    slug: &RepoSlug,
    branch: &str,
) -> Result<Option<CopilotCodeReviewParams>, GhError> {
    let summaries = fetch_ruleset_list(slug)?;
    if summaries.is_empty() {
        return Ok(None);
    }

    thread::scope(|s| {
        let handles: Vec<_> = summaries
            .into_iter()
            .map(|summary| s.spawn(move || extract_copilot(slug, summary.id, branch)))
            .collect();

        for h in handles {
            if let Some(params) = h.join().expect("fetch_ruleset panicked")? {
                return Ok(Some(params));
            }
        }
        Ok(None)
    })
}

fn extract_copilot(
    slug: &RepoSlug,
    id: u64,
    branch: &str,
) -> Result<Option<CopilotCodeReviewParams>, GhError> {
    let ruleset = match fetch_ruleset(slug, id) {
        Ok(r) => r,
        Err(GhError::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(extract_copilot_from_ruleset(ruleset, branch))
}

/// Pure projection of a rule source into reviewer-axis parameters.
/// Split from the fetch path so the active/admit/parameter-decode
/// branches are unit-testable without subprocess.
pub(crate) fn extract_copilot_from_ruleset(
    ruleset: Ruleset,
    branch: &str,
) -> Option<CopilotCodeReviewParams> {
    if ruleset.enforcement != RulesetEnforcement::Active {
        return None;
    }
    if !ruleset_matches_branch(ruleset.conditions.as_ref(), branch) {
        return None;
    }
    for rule in ruleset.rules {
        if rule.rule_type != "copilot_code_review" {
            continue;
        }
        // Absent parameters payload → host-default values per the
        // module-level invariant. Skipping such rules would
        // misreport default-configured rule sources as absent.
        let parsed = match rule.parameters {
            Some(p) => match serde_json::from_value::<CopilotCodeReviewParams>(p) {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => CopilotCodeReviewParams {
                review_on_push: false,
                review_draft_pull_requests: false,
            },
        };
        return Some(parsed);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::rulesets::{RefNameCondition, RulesetConditions, RulesetRule};
    use serde_json::json;

    fn ruleset(
        enforcement: RulesetEnforcement,
        conditions: Option<RulesetConditions>,
        rules: Vec<RulesetRule>,
    ) -> Ruleset {
        Ruleset {
            id: 1,
            name: "rs".to_string(),
            enforcement,
            conditions,
            rules,
        }
    }

    fn copilot_rule(params: Option<serde_json::Value>) -> RulesetRule {
        RulesetRule {
            rule_type: "copilot_code_review".to_string(),
            parameters: params,
        }
    }

    fn other_rule() -> RulesetRule {
        RulesetRule {
            rule_type: "required_status_checks".to_string(),
            parameters: None,
        }
    }

    fn match_all_conditions() -> RulesetConditions {
        // ~ALL == default include `~ALL`, no excludes. Matches every branch.
        RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec!["~ALL".to_string()],
                exclude: vec![],
            }),
        }
    }

    #[test]
    fn extract_from_ruleset_returns_none_when_disabled() {
        let rs = ruleset(
            RulesetEnforcement::Disabled,
            Some(match_all_conditions()),
            vec![copilot_rule(None)],
        );
        assert!(extract_copilot_from_ruleset(rs, "master").is_none());
    }

    #[test]
    fn extract_from_ruleset_returns_none_for_branch_outside_scope() {
        let rs = ruleset(
            RulesetEnforcement::Active,
            Some(RulesetConditions {
                ref_name: Some(RefNameCondition {
                    include: vec!["refs/heads/release/*".to_string()],
                    exclude: vec![],
                }),
            }),
            vec![copilot_rule(None)],
        );
        assert!(extract_copilot_from_ruleset(rs, "master").is_none());
    }

    #[test]
    fn extract_from_ruleset_returns_defaults_when_parameters_missing() {
        // Missing parameters → defaults. The regression-prone path
        // called out at line 78 of the production source.
        let rs = ruleset(
            RulesetEnforcement::Active,
            Some(match_all_conditions()),
            vec![copilot_rule(None)],
        );
        let params = extract_copilot_from_ruleset(rs, "master").unwrap();
        assert!(!params.review_on_push);
        assert!(!params.review_draft_pull_requests);
    }

    #[test]
    fn extract_from_ruleset_parses_parameters_when_present() {
        let rs = ruleset(
            RulesetEnforcement::Active,
            Some(match_all_conditions()),
            vec![copilot_rule(Some(json!({
                "review_on_push": true,
                "review_draft_pull_requests": true,
            })))],
        );
        let params = extract_copilot_from_ruleset(rs, "master").unwrap();
        assert!(params.review_on_push);
        assert!(params.review_draft_pull_requests);
    }

    #[test]
    fn extract_from_ruleset_skips_non_copilot_rules() {
        let rs = ruleset(
            RulesetEnforcement::Active,
            Some(match_all_conditions()),
            vec![other_rule()],
        );
        assert!(extract_copilot_from_ruleset(rs, "master").is_none());
    }

    #[test]
    fn extract_from_ruleset_skips_rule_with_unparseable_parameters_and_continues() {
        // First copilot rule has garbage params → continue; second has
        // valid params → returned. Locks the "skip on parse error,
        // don't bail" branch.
        let rs = ruleset(
            RulesetEnforcement::Active,
            Some(match_all_conditions()),
            vec![
                copilot_rule(Some(json!({"review_on_push": "not-a-bool"}))),
                copilot_rule(Some(json!({
                    "review_on_push": true,
                    "review_draft_pull_requests": false,
                }))),
            ],
        );
        let params = extract_copilot_from_ruleset(rs, "master").unwrap();
        assert!(params.review_on_push);
        assert!(!params.review_draft_pull_requests);
    }

    #[test]
    fn extract_from_ruleset_returns_none_when_no_conditions_present() {
        // Conditions absent → ruleset_matches_branch returns true
        // (no scope restriction). Validate the active+matching path
        // even without an explicit conditions block.
        let rs = ruleset(RulesetEnforcement::Active, None, vec![copilot_rule(None)]);
        let params = extract_copilot_from_ruleset(rs, "master").unwrap();
        assert!(!params.review_on_push);
    }
}
