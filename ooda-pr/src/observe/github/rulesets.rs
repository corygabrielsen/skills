//! Typed view of the GitHub rulesets endpoints:
//!
//!   - `GET /repos/{o}/{r}/rulesets?per_page=100` — list of summaries
//!   - `GET /repos/{o}/{r}/rulesets/{id}`         — full ruleset
//!
//! Rule shapes vary by `type` and new types may appear over time.
//! `RulesetRule.parameters` is kept as `serde_json::Value` so unknown
//! rule types do not break deserialization; typed parameter structs
//! (`CopilotCodeReviewParams`, `RequiredStatusChecksParams`) deserialize
//! opt-in from the `parameters` field when callers need them.

use serde::Deserialize;

use crate::ids::{CheckName, RepoSlug};

use super::gh::{gh_json, gh_json_paginate, GhError};

/// Fetch every ruleset summary for a repo. Uses `--paginate` with
/// `per_page=100`. Repos with >100 rulesets would otherwise drop
/// the active Copilot ruleset on a later page, leaving
/// `fetch_copilot_config` returning `None` and PRs misreported as
/// Copilot-unconfigured. Class invariant: every list endpoint that
/// may exceed one page uses `gh_json_paginate`.
pub fn fetch_ruleset_list(slug: &RepoSlug) -> Result<Vec<RulesetSummary>, GhError> {
    let path = format!("repos/{slug}/rulesets?per_page=100");
    gh_json_paginate(&["api", "--paginate", &path])
}

/// Fetch a single ruleset by id.
///
/// Callers that iterate the ruleset list may see a ruleset vanish
/// between list and fetch; this surfaces as `GhError::NotFound` and
/// should be treated as a non-fatal skip.
pub fn fetch_ruleset(slug: &RepoSlug, id: u64) -> Result<Ruleset, GhError> {
    let path = format!("repos/{slug}/rulesets/{id}");
    gh_json(&["api", &path])
}

/// Entry from the list endpoint. The list response is trimmed; the
/// full shape is fetched per-id via `Ruleset`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RulesetSummary {
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Ruleset {
    pub id: u64,
    pub name: String,
    pub enforcement: RulesetEnforcement,
    pub rules: Vec<RulesetRule>,
    /// Branch-scoping conditions. `None` (or missing in JSON)
    /// means the ruleset applies to all refs.
    #[serde(default)]
    pub conditions: Option<RulesetConditions>,
}

/// Branch-scoping conditions on a ruleset. Currently only
/// `ref_name` is modeled; other condition kinds (repository
/// property, etc.) are silently ignored — repos that depend on
/// them appear unconditionally matched.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct RulesetConditions {
    #[serde(default)]
    pub ref_name: Option<RefNameCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct RefNameCondition {
    /// Patterns: `~ALL` (every branch), `~DEFAULT_BRANCH`
    /// (repo default), exact `refs/heads/<name>`, or
    /// fnmatch-style globs like `refs/heads/release/*`.
    #[serde(default)]
    pub include: Vec<String>,
    /// Same pattern grammar; matches here override `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// True iff the ruleset's branch-scope conditions cover `branch`.
///
/// Resolution rules (first match wins):
///   1. No conditions or no `ref_name` → covers all branches.
///   2. Any `exclude` pattern matches `branch` → false.
///   3. Empty `include` (only excludes) → true.
///   4. Any `include` pattern matches `branch` → true.
///   5. Otherwise → false.
///
/// Pattern grammar (best-effort):
///   - `~ALL` matches every branch.
///   - `~DEFAULT_BRANCH` conservatively matches (we don't fetch
///     the repo's default branch separately; better to over-
///     report Copilot as configured than to silently drop it).
///   - `refs/heads/<exact>` exact match.
///   - `refs/heads/<glob>` with `*` wildcards (fnmatch-style,
///     no character classes).
pub fn ruleset_matches_branch(
    conditions: Option<&RulesetConditions>,
    branch: &str,
) -> bool {
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
    match pat {
        "~ALL" => true,
        "~DEFAULT_BRANCH" => true, // conservative: we don't know the default
        _ if pat.contains('*') => glob_matches(pat, qualified_ref),
        _ => pat == qualified_ref,
    }
}

/// Minimal `*`-only glob matcher. Splits on `*` and walks the
/// segments left-to-right against the input. Empty leading/
/// trailing segments anchor each end (so `refs/heads/release/*`
/// matches `refs/heads/release/1.2` but not `release/1.2`).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RulesetEnforcement {
    Active,
    Disabled,
    Evaluate,
}

/// A single rule inside a ruleset. `parameters` is raw JSON — opt into
/// a typed view via `serde_json::from_value(rule.parameters.clone())`
/// using one of the typed `*Params` structs below.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RulesetRule {
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

/// Shape of `parameters` when `rule_type == "copilot_code_review"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct CopilotCodeReviewParams {
    pub review_on_push: bool,
    pub review_draft_pull_requests: bool,
}

/// Shape of `parameters` when `rule_type == "required_status_checks"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RequiredStatusChecksParams {
    pub required_status_checks: Vec<RequiredStatusCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RequiredStatusCheck {
    pub context: CheckName,
    /// Null when the required check is not pinned to a GitHub App
    /// (status posted from any source matching the context name).
    /// Modeling as `u64` would silently drop the entire rule.
    pub integration_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_FIXTURE: &str =
        include_str!("../../../test/fixtures/github/rulesets_list.json");
    const DETAIL_FIXTURE: &str =
        include_str!("../../../test/fixtures/github/ruleset_detail.json");
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
        assert_eq!(r.id, 12663934);
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
            let json = format!(
                r#"{{"id":1,"name":"r","enforcement":"{s}","rules":[]}}"#,
            );
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
