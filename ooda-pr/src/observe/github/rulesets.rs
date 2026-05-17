//! Typed projection of the rule-source endpoints (summary list +
//! per-id detail).
//!
//! # Invariants
//!
//! - **Polymorphic-parameter tolerance**: rule shapes vary by type
//!   and new types may appear over time. Parameter payloads are
//!   carried as opaque JSON; typed projections deserialize on
//!   demand so unknown rule types never break decoding.
//! - **Branch-scoping is a separate predicate**: a rule is "active
//!   for this branch" only when both its enforcement state and its
//!   scoping conditions admit the branch.

use serde::{Deserialize, Serialize};

use crate::ids::{CheckName, RepoSlug};

use super::gh::{GhError, gh_json, gh_json_paginate};

/// Fetch every rule-source summary for a repo. Pagination is
/// fetcher-side: dropping later pages would silently remove rule
/// sources from the decision model.
pub(crate) fn fetch_ruleset_list(slug: &RepoSlug) -> Result<Vec<RulesetSummary>, GhError> {
    let path = format!("repos/{slug}/rulesets?per_page=100");
    gh_json_paginate(&["api", "--paginate", &path])
}

/// Fetch a single rule-source detail by id. Disappearance between
/// list and detail (rule-source removed mid-iteration) surfaces as
/// the dedicated not-found variant; callers treat it as a non-
/// fatal skip.
pub(crate) fn fetch_ruleset(slug: &RepoSlug, id: u64) -> Result<Ruleset, GhError> {
    let path = format!("repos/{slug}/rulesets/{id}");
    gh_json(&["api", &path])
}

/// Summary row from the list endpoint. The full shape is fetched
/// per-id on demand.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RulesetSummary {
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Ruleset {
    pub id: u64,
    pub name: String,
    pub enforcement: RulesetEnforcement,
    pub rules: Vec<RulesetRule>,
    /// Branch-scoping conditions. Absence means unconditionally
    /// applicable across all refs.
    #[serde(default)]
    pub conditions: Option<RulesetConditions>,
}

/// Branch-scoping conditions. Only the ref-name conditions are
/// modeled; other condition kinds present in the host vocabulary
/// would route to unconditional match — accept the over-report
/// rather than silently drop the rule source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) struct RulesetConditions {
    #[serde(default)]
    pub ref_name: Option<RefNameCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub(crate) struct RefNameCondition {
    /// Inclusion patterns; see the pattern grammar in the matcher.
    #[serde(default)]
    pub include: Vec<String>,
    /// Exclusion patterns; matches override inclusion.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Predicate: do the conditions admit `branch`?
///
/// Resolution rules (first match wins):
///   1. Conditions absent → unconditionally admits.
///   2. Any exclusion matches → reject.
///   3. Inclusions empty → unconditionally admits (excludes only).
///   4. Any inclusion matches → admit.
///   5. Otherwise → reject.
///
/// Pattern grammar:
///   - Wildcard literals match everything; default-branch literal
///     admits conservatively (no separate default-branch fetch).
///   - Exact fully-qualified ref matches by equality.
///   - Glob-bearing patterns match by anchored splat segments.
pub(crate) fn ruleset_matches_branch(conditions: Option<&RulesetConditions>, branch: &str) -> bool {
    let Some(c) = conditions else { return true };
    let Some(rn) = &c.ref_name else { return true };
    let qualified = format!("refs/heads/{branch}");
    for pat in &rn.exclude {
        if ref_pattern_matches(pat, &qualified) {
            return false;
        }
    }
    if rn.include.is_empty() {
        return true;
    }
    rn.include
        .iter()
        .any(|pat| ref_pattern_matches(pat, &qualified))
}

fn ref_pattern_matches(pat: &str, qualified_ref: &str) -> bool {
    // Arms kept distinct per the wildcard-pattern vocabulary even
    // when they collapse — preserves spec clarity at the boundary.
    #[allow(clippy::match_same_arms)]
    match pat {
        "~ALL" => true,
        "~DEFAULT_BRANCH" => true, // conservative: default unknown
        _ if pat.contains('*') => glob_matches(pat, qualified_ref),
        _ => pat == qualified_ref,
    }
}

/// Splat-only glob matcher. Splits on the splat character and walks
/// segments left-to-right; empty leading/trailing segments anchor
/// each end.
fn glob_matches(pat: &str, s: &str) -> bool {
    let segments: Vec<&str> = pat.split('*').collect();
    if segments.len() == 1 {
        return pat == s;
    }
    let mut cursor = 0;
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            if !s[cursor..].starts_with(seg) {
                return false;
            }
            cursor += seg.len();
        } else if i == segments.len() - 1 {
            return s[cursor..].ends_with(seg);
        } else {
            match s[cursor..].find(seg) {
                Some(pos) => cursor += pos + seg.len(),
                None => return false,
            }
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RulesetEnforcement {
    Active,
    Disabled,
    Evaluate,
}

/// One rule. The parameters payload is opaque JSON; typed payload
/// projections deserialize on demand for known rule types.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RulesetRule {
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

/// Typed parameter projection for the reviewer-axis rule type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct CopilotCodeReviewParams {
    pub review_on_push: bool,
    pub review_draft_pull_requests: bool,
}

/// Typed parameter projection for the required-checks rule type.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RequiredStatusChecksParams {
    pub required_status_checks: Vec<RequiredStatusCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RequiredStatusCheck {
    pub context: CheckName,
    /// Absent when the check is not pinned to a host app (status
    /// accepted from any source matching the context). Modeling as
    /// non-optional would silently drop the entire rule.
    pub integration_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_FIXTURE: &str = include_str!("../../../test/fixtures/github/rulesets_list.json");
    const DETAIL_FIXTURE: &str = include_str!("../../../test/fixtures/github/ruleset_detail.json");
    const DETAIL_WITH_COPILOT: &str =
        include_str!("../../../test/fixtures/github/ruleset_with_copilot.json");

    #[test]
    fn deserializes_list_summary() {
        let list: Vec<RulesetSummary> = serde_json::from_str(LIST_FIXTURE).unwrap();
        assert_eq!(list.len(), 7);
        assert!(list.iter().all(|s| s.id > 0));
    }

    #[test]
    fn deserializes_non_copilot_detail() {
        let r: Ruleset = serde_json::from_str(DETAIL_FIXTURE).unwrap();
        assert_eq!(r.id, 12_663_934);
        assert_eq!(r.name, "Ban bare branch prefix names");
        assert_eq!(r.enforcement, RulesetEnforcement::Active);
        assert_eq!(r.rules.len(), 2);
        // These rule types have no parameters field.
        assert_eq!(r.rules[0].rule_type, "creation");
        assert_eq!(r.rules[0].parameters, None);
    }

    #[test]
    fn deserializes_copilot_detail_and_extracts_params() {
        let r: Ruleset = serde_json::from_str(DETAIL_WITH_COPILOT).unwrap();
        assert_eq!(r.enforcement, RulesetEnforcement::Active);
        assert_eq!(r.rules.len(), 1);
        let rule = &r.rules[0];
        assert_eq!(rule.rule_type, "copilot_code_review");

        let params_raw = rule.parameters.as_ref().expect("copilot rule has params");
        let params: CopilotCodeReviewParams = serde_json::from_value(params_raw.clone()).unwrap();
        assert!(!params.review_on_push);
        assert!(!params.review_draft_pull_requests);
    }

    #[test]
    fn unknown_rule_type_does_not_break_deserialization() {
        let json = r#"{
            "id": 1,
            "name": "r",
            "enforcement": "active",
            "rules": [{"type":"future_rule_type_we_invent_later","parameters":{"x":42}}]
        }"#;
        let r: Ruleset = serde_json::from_str(json).unwrap();
        assert_eq!(r.rules[0].rule_type, "future_rule_type_we_invent_later");
        assert!(r.rules[0].parameters.is_some());
    }

    #[test]
    fn enforcement_variants_parse() {
        for (s, expected) in [
            ("active", RulesetEnforcement::Active),
            ("disabled", RulesetEnforcement::Disabled),
            ("evaluate", RulesetEnforcement::Evaluate),
        ] {
            let json = format!(r#"{{"id":1,"name":"r","enforcement":"{s}","rules":[]}}"#);
            let r: Ruleset = serde_json::from_str(&json).unwrap();
            assert_eq!(r.enforcement, expected);
        }
    }

    #[test]
    fn ruleset_matches_branch_no_conditions_matches_all() {
        assert!(ruleset_matches_branch(None, "feature-x"));
        assert!(ruleset_matches_branch(
            Some(&RulesetConditions::default()),
            "feature-x"
        ));
    }

    #[test]
    fn ruleset_matches_branch_all_wildcard() {
        let c = RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec!["~ALL".into()],
                exclude: vec![],
            }),
        };
        assert!(ruleset_matches_branch(Some(&c), "any-branch"));
    }

    #[test]
    fn ruleset_matches_branch_exact_match() {
        let c = RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec!["refs/heads/master".into()],
                exclude: vec![],
            }),
        };
        assert!(ruleset_matches_branch(Some(&c), "master"));
        assert!(!ruleset_matches_branch(Some(&c), "feature-x"));
    }

    #[test]
    fn ruleset_matches_branch_glob() {
        let c = RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec!["refs/heads/release/*".into()],
                exclude: vec![],
            }),
        };
        assert!(ruleset_matches_branch(Some(&c), "release/1.2"));
        assert!(ruleset_matches_branch(Some(&c), "release/v2"));
        assert!(!ruleset_matches_branch(Some(&c), "feature-x"));
        // No partial-prefix match without the trailing glob.
        assert!(!ruleset_matches_branch(Some(&c), "release"));
    }

    #[test]
    fn ruleset_matches_branch_exclude_overrides_include() {
        let c = RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec!["~ALL".into()],
                exclude: vec!["refs/heads/wip/*".into()],
            }),
        };
        assert!(ruleset_matches_branch(Some(&c), "master"));
        assert!(!ruleset_matches_branch(Some(&c), "wip/feature"));
    }

    #[test]
    fn ruleset_matches_branch_default_branch_conservative() {
        // We don't fetch the repo's default branch — better to
        // over-report Copilot as configured than to silently drop it.
        let c = RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec!["~DEFAULT_BRANCH".into()],
                exclude: vec![],
            }),
        };
        assert!(ruleset_matches_branch(Some(&c), "any-branch"));
    }

    #[test]
    fn ruleset_matches_branch_only_excludes_implies_include_all() {
        let c = RulesetConditions {
            ref_name: Some(RefNameCondition {
                include: vec![],
                exclude: vec!["refs/heads/wip/*".into()],
            }),
        };
        assert!(ruleset_matches_branch(Some(&c), "master"));
        assert!(!ruleset_matches_branch(Some(&c), "wip/x"));
    }

    #[test]
    fn ruleset_with_conditions_round_trips() {
        let json = r#"{
            "id": 1,
            "name": "r",
            "enforcement": "active",
            "rules": [],
            "conditions": {
                "ref_name": {
                    "include": ["refs/heads/master"],
                    "exclude": []
                }
            }
        }"#;
        let r: Ruleset = serde_json::from_str(json).unwrap();
        assert!(r.conditions.is_some());
        assert!(ruleset_matches_branch(r.conditions.as_ref(), "master"));
    }

    #[test]
    fn required_status_checks_params_parse() {
        let json = r#"{
            "required_status_checks": [
                {"context": "Lint", "integration_id": 15368},
                {"context": "Unit Tests", "integration_id": 15368}
            ]
        }"#;
        let params: RequiredStatusChecksParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.required_status_checks.len(), 2);
        assert_eq!(params.required_status_checks[0].context.as_str(), "Lint");
        assert_eq!(params.required_status_checks[0].integration_id, Some(15368));
    }
}
