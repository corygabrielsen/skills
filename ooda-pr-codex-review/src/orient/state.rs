//! Project PR view + branch-rule context into the typed state axis.
//!
//! # Invariants
//!
//! - **Single observation source per field**: every projected field
//!   reads exactly one observation source. Rule-typing plumbing stays
//!   at the boundary; this module never re-parses ruleset parameters
//!   that the caller already projected.
//! - **Stack-topology vs merge-state separation**: stack-topology
//!   facts (e.g., open-parent presence) come from the resolved stack
//!   root; merge-state facts come from the host's merge-state field.
//!   The two are surfaced as orthogonal projections; do not collapse.
//! - **Title budget includes auto-suffix**: title length accounts for
//!   the stack-tooling auto-appendage so the gate matches what lands
//!   after submission, not what was typed.

use std::collections::HashSet;

use crate::ids::{BranchName, GitCommitSha, Timestamp};
use crate::observe::github::branch_protection::BranchProtection;
use crate::observe::github::branch_rules::BranchRule;
use crate::observe::github::checks::PullRequestCheck;
use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable, PullRequestView};
use crate::observe::github::rulesets::RequiredStatusChecksParams;
use serde::Serialize;

const TITLE_MAX_LEN: usize = 50;
/// The label string both orient (detects) and act (removes) must
/// agree on. Public so act/* can reference exactly the same value.
pub(crate) const WIP_LABEL: &str = "work in progress";
const MERGE_WHEN_READY_LABEL: &str = "merge-when-ready";
const CONTENT_LABELS: &[&str] = &["bug", "enhancement"];

// Each bool represents a distinct mergeability axis; restructuring
// would obscure the GitHub API mapping.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PullRequestProjection {
    pub conflict: Mergeable,
    pub draft: bool,
    pub wip: bool,
    pub title_len: usize,
    pub title_ok: bool,
    pub body: bool,
    pub summary: bool,
    pub test_plan: bool,
    pub content_label: bool,
    pub assignees: usize,
    pub reviewers: usize,
    pub merge_when_ready: bool,
    pub commits: usize,
    /// Base advanced past the merge base; rebase needed for stack
    /// hygiene.
    pub behind: bool,
    /// PR is stacked atop another open PR. Distinct from merge state:
    /// stack-topology gates (e.g., parent-must-merge-first checks)
    /// fire here even when merge state is otherwise clean, and a
    /// rebase that ignores topology orphans dependent branches.
    pub has_open_parent_pr: bool,
    /// Host's full merge-state field. Preserved (not collapsed) so
    /// decide can surface unmodeled gates — custom rulesets,
    /// deployment protection — that would otherwise let the loop
    /// halt Success while the host still blocks merge.
    pub merge_state_status: MergeStateStatus,
    pub updated_at: Timestamp,
    /// HEAD commit author timestamp; absent when unobservable.
    pub last_commit_at: Option<Timestamp>,
    /// Sorted, deduped rule-type identifiers active on the resolved
    /// target branch. Drives the fallback prompt's enumeration of
    /// candidate gates when no modeled axis explains a merge block.
    pub active_branch_rule_types: Vec<String>,
    /// Check contexts required by rule-source declarations on the
    /// target branch. Subset of, not aggregated with, the legacy-
    /// source required checks — that union lives in a separate
    /// projection.
    pub required_check_names_per_ruleset: Vec<String>,
    /// Required contexts with no run of matching name on HEAD.
    /// Presence is name-equality only; conclusion is ignored —
    /// pass, fail, pending all count as present.
    pub missing_required_check_names_on_head: Vec<String>,
    /// True iff GitHub will gate merge on review-thread resolution.
    /// Union of legacy-protection's `required_conversation_resolution`
    /// and any active `pull_request` ruleset rule's
    /// `required_review_thread_resolution` parameter. Drives the
    /// merge-eligibility decision; outdated unresolved threads still
    /// satisfy this gate even though they're not agent-addressable.
    pub conversation_resolution_required: bool,
    /// True iff GitHub will gate merge on signed commits. Union of
    /// legacy-protection's `required_signatures` and any active
    /// `required_signatures`-typed ruleset rule. Signal only;
    /// remediation is human-only.
    pub signatures_required: bool,
    /// Commit SHAs on the PR whose GitHub signing verification
    /// reports `verified=false`. Empty when every commit is
    /// verified. The `SigningEligibility` closure-check axis
    /// gates on `signatures_required && !unsigned_commits.is_empty()`
    /// and emits a Pathology `HandoffHuman` when both hold.
    pub unsigned_commits: Vec<GitCommitSha>,
    /// Minimum APPROVED reviews the branch ruleset requires on
    /// HEAD. `Some(0)` distinguishes "rule present, requires zero"
    /// from `None` ("no rule present"). Union across active
    /// `pull_request` ruleset rules; takes the maximum (most
    /// restrictive) when multiple rules layer.
    pub required_approving_review_count: Option<u32>,
    /// `true` iff any active branch ruleset has a
    /// `copilot_code_review` rule. Signals that Copilot must
    /// review (and approve, per the rule's semantic) the PR before
    /// merge. Consumed by `merge_eligibility` alongside the
    /// actual Copilot review state to drive a named gate instead
    /// of the unmodeled-policy Pathology fallback.
    pub copilot_review_required: bool,
}

/// Project a PR view + branch-rule context into the state axis.
///
/// `last_commit_at` crosses an observation boundary (separate source
/// from the PR view) and is supplied by the caller. `stack_root` is
/// the resolved protected base; inequality with the PR's immediate
/// base witnesses an unmerged parent PR. `branch_rules` and
/// `head_checks` seed the rule-source projections.
pub(crate) fn orient_state(
    pr: &PullRequestView,
    last_commit_at: Option<Timestamp>,
    stack_root: &BranchName,
    branch_rules: &[BranchRule],
    head_checks: &[PullRequestCheck],
    protection: Option<&BranchProtection>,
    unsigned_commits: Vec<GitCommitSha>,
) -> PullRequestProjection {
    let body = pr.body.as_deref().unwrap_or_default();
    let label_names: Vec<&str> = pr.labels.iter().map(|l| l.name.as_str()).collect();

    // Title-length budget covers the stack-tooling auto-suffix that
    // lands on submit, so the gate matches what merges, not what was
    // typed.
    let suffix_len = format!(" (#{})", pr.number).len();
    let title_len = pr.title.chars().count() + suffix_len;

    let active_branch_rule_types = sorted_dedup_rule_types(branch_rules);
    let required_check_names_per_ruleset = required_check_names_per_ruleset(branch_rules);
    let missing_required_check_names_on_head =
        missing_on_head(&required_check_names_per_ruleset, head_checks);
    let conversation_resolution_required =
        conversation_resolution_required(branch_rules, protection);
    let signatures_required = signatures_required(branch_rules, protection);
    let required_approving_review_count = required_approving_review_count(branch_rules);
    let copilot_review_required = copilot_review_required(branch_rules);

    PullRequestProjection {
        conflict: pr.mergeable,
        draft: pr.is_draft,
        wip: label_names.contains(&WIP_LABEL),
        title_len,
        title_ok: title_len <= TITLE_MAX_LEN,
        body: !body.is_empty(),
        summary: has_section_heading(body, "Summary"),
        test_plan: has_section_heading(body, "Test"),
        content_label: CONTENT_LABELS.iter().any(|c| label_names.contains(c)),
        assignees: pr.assignees.len(),
        reviewers: pr.review_requests.len(),
        merge_when_ready: label_names.contains(&MERGE_WHEN_READY_LABEL),
        commits: pr.commits.len(),
        behind: matches!(pr.merge_state_status, MergeStateStatus::Behind),
        has_open_parent_pr: &pr.base_ref_name != stack_root,
        merge_state_status: pr.merge_state_status,
        updated_at: pr.updated_at,
        last_commit_at,
        active_branch_rule_types,
        required_check_names_per_ruleset,
        missing_required_check_names_on_head,
        conversation_resolution_required,
        signatures_required,
        unsigned_commits,
        required_approving_review_count,
        copilot_review_required,
    }
}

/// `true` iff GitHub will gate merge on review-thread resolution
/// from any active source.
///
/// Sources, taken as a union (most-restrictive wins):
///   - Legacy branch protection's `required_conversation_resolution`.
///   - Any active branch ruleset's `pull_request` rule with
///     parameter `required_review_thread_resolution == true`.
///
/// Reads by rule type and parameter shape only — no rule-name
/// pattern matching, no slug-coupling.
fn conversation_resolution_required(
    branch_rules: &[BranchRule],
    protection: Option<&BranchProtection>,
) -> bool {
    if protection.is_some_and(BranchProtection::conversation_resolution_enabled) {
        return true;
    }
    branch_rules.iter().any(|r| {
        if r.rule_type != "pull_request" {
            return false;
        }
        r.parameters
            .as_ref()
            .and_then(|p| p.get("required_review_thread_resolution"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

/// Minimum APPROVED reviews the branch ruleset requires on HEAD.
/// `Some(0)` distinguishes "rule present, requires zero" from
/// `None` ("no rule present"). Reads from every active
/// `pull_request` ruleset rule's `required_approving_review_count`
/// parameter, returning the maximum (most-restrictive union).
fn required_approving_review_count(branch_rules: &[BranchRule]) -> Option<u32> {
    branch_rules
        .iter()
        .filter_map(|r| {
            if r.rule_type != "pull_request" {
                return None;
            }
            r.parameters
                .as_ref()?
                .get("required_approving_review_count")?
                .as_u64()
        })
        .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
        .max()
}

/// `true` iff any active branch ruleset has a `copilot_code_review`
/// rule. Signals that Copilot must review (and approve) the PR
/// before the merge gate clears.
fn copilot_review_required(branch_rules: &[BranchRule]) -> bool {
    branch_rules
        .iter()
        .any(|r| r.rule_type == "copilot_code_review")
}

/// `true` iff GitHub will gate merge on signed commits from any
/// active source.
///
/// Sources, taken as a union:
///   - Legacy branch protection's `required_signatures.enabled`.
///   - Any active branch ruleset rule of type `required_signatures`.
fn signatures_required(branch_rules: &[BranchRule], protection: Option<&BranchProtection>) -> bool {
    if protection.is_some_and(BranchProtection::signatures_enabled) {
        return true;
    }
    branch_rules
        .iter()
        .any(|r| r.rule_type == "required_signatures")
}

fn sorted_dedup_rule_types(branch_rules: &[BranchRule]) -> Vec<String> {
    let mut out: Vec<String> = branch_rules.iter().map(|r| r.rule_type.clone()).collect();
    out.sort();
    out.dedup();
    out
}

fn required_check_names_per_ruleset(branch_rules: &[BranchRule]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
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
            let name = c.context.as_str().to_owned();
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }
    out
}

fn missing_on_head(required: &[String], head_checks: &[PullRequestCheck]) -> Vec<String> {
    let present: HashSet<&str> = head_checks.iter().map(|c| c.name.as_str()).collect();
    required
        .iter()
        .filter(|name| !present.contains(name.as_str()))
        .cloned()
        .collect()
}

/// True iff any line begins with `## <heading>` (case-insensitive on
/// the heading text; `##` anchored at line start). Inline mentions in
/// prose do not satisfy the predicate.
fn has_section_heading(body: &str, heading: &str) -> bool {
    let needle = format!("## {}", heading.to_ascii_lowercase());
    body.lines()
        .any(|line| line.to_ascii_lowercase().starts_with(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, GitHubLogin, PullRequestNumber};
    use crate::observe::github::pull_request_view::{
        Assignee, Commit, Label, PullRequestState, PullRequestView, ReviewRequest,
    };

    fn pull_request_view(overrides: impl FnOnce(&mut PullRequestView)) -> PullRequestView {
        let mut v = PullRequestView {
            title: "fix thing".into(),
            number: PullRequestNumber::new(123).unwrap(),
            url: "https://example/pr/123".into(),
            body: Some("body".into()),
            state: PullRequestState::Open,
            is_draft: false,
            mergeable: Mergeable::Mergeable,
            merge_state_status: MergeStateStatus::Clean,
            head_ref_oid: GitCommitSha::parse(&"a".repeat(40)).unwrap(),
            head_ref_name: crate::ids::BranchName::parse("feat/branch").unwrap(),
            base_ref_name: crate::ids::BranchName::parse("master").unwrap(),
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            closed_at: None,
            merged_at: None,
            review_decision: None,
            labels: vec![],
            assignees: vec![],
            review_requests: vec![],
            commits: vec![],
            author: None,
        };
        overrides(&mut v);
        v
    }

    fn label(name: &str) -> Label {
        Label {
            name: name.to_owned(),
        }
    }

    #[test]
    fn defaults_for_minimal_open_pull_request() {
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.conflict, Mergeable::Mergeable);
        assert!(!s.draft);
        assert!(!s.wip);
        assert!(s.body);
        assert!(!s.summary);
        assert!(!s.test_plan);
        assert!(!s.content_label);
        assert_eq!(s.assignees, 0);
        assert_eq!(s.reviewers, 0);
        assert!(!s.merge_when_ready);
        assert_eq!(s.commits, 0);
        assert!(!s.behind);
        assert_eq!(s.last_commit_at, None);
    }

    #[test]
    fn title_len_includes_phantom_pull_request_suffix() {
        // "fix thing" = 9, " (#123)" = 7 → 16
        let pr = pull_request_view(|p| p.title = "fix thing".into());
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.title_len, 16);
        assert!(s.title_ok);
    }

    #[test]
    fn title_just_over_50_with_suffix_fails() {
        // Need title_len > 50. Suffix " (#123)" is 7 chars, so a
        // 44-char title gives total 51.
        let pr = pull_request_view(|p| p.title = "a".repeat(44));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.title_len, 51);
        assert!(!s.title_ok);
    }

    #[test]
    fn title_at_exactly_50_passes() {
        let pr = pull_request_view(|p| p.title = "a".repeat(43));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.title_len, 50);
        assert!(s.title_ok);
    }

    #[test]
    fn empty_body_marks_body_false_and_no_sections() {
        let pr = pull_request_view(|p| p.body = Some(String::new()));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.body);
        assert!(!s.summary);
        assert!(!s.test_plan);
    }

    #[test]
    fn null_body_treated_as_empty() {
        let pr = pull_request_view(|p| p.body = None);
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.body);
    }

    #[test]
    fn detects_summary_and_test_plan_headings_case_insensitive() {
        let pr = pull_request_view(|p| {
            p.body = Some("## summary\nstuff\n\n## TEST PLAN\n- check it\n".into());
        });
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(s.summary);
        assert!(s.test_plan);
    }

    #[test]
    fn heading_must_be_at_line_start() {
        // Inline mention shouldn't trigger the heading detector.
        let pr = pull_request_view(|p| {
            p.body = Some("intro about ## summary in prose".into());
        });
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.summary);
    }

    #[test]
    fn wip_label_detected_exact_match() {
        let pr = pull_request_view(|p| p.labels.push(label("work in progress")));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(s.wip);
    }

    #[test]
    fn merge_when_ready_label_detected() {
        let pr = pull_request_view(|p| p.labels.push(label("merge-when-ready")));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(s.merge_when_ready);
    }

    #[test]
    fn content_label_recognizes_bug_or_enhancement() {
        for ct in ["bug", "enhancement"] {
            let pr = pull_request_view(|p| p.labels.push(label(ct)));
            let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
            assert!(s.content_label, "expected content_label for {ct}");
        }
        // A non-content label doesn't count.
        let pr = pull_request_view(|p| p.labels.push(label("documentation")));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.content_label);
    }

    #[test]
    fn behind_only_when_merge_state_status_is_behind() {
        let pr = pull_request_view(|p| p.merge_state_status = MergeStateStatus::Behind);
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(s.behind);

        let pr = pull_request_view(|p| p.merge_state_status = MergeStateStatus::Blocked);
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.behind);
    }

    #[test]
    fn counts_assignees_reviewers_and_commits() {
        let pr = pull_request_view(|p| {
            p.assignees = vec![Assignee {
                login: GitHubLogin::parse("alice").unwrap(),
            }];
            p.review_requests = vec![ReviewRequest {
                login: Some(GitHubLogin::parse("bob").unwrap()),
                name: None,
            }];
            p.commits = vec![
                Commit {
                    oid: GitCommitSha::parse(&"a".repeat(40)).unwrap(),
                    committed_date: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
                },
                Commit {
                    oid: GitCommitSha::parse(&"b".repeat(40)).unwrap(),
                    committed_date: Timestamp::parse("2026-04-23T11:00:00Z").unwrap(),
                },
            ];
        });
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.assignees, 1);
        assert_eq!(s.reviewers, 1);
        assert_eq!(s.commits, 2);
    }

    #[test]
    fn last_commit_at_passes_through() {
        let ts = Timestamp::parse("2026-04-23T11:00:00Z").unwrap();
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, Some(ts), &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.last_commit_at, Some(ts));
    }

    fn branch_rule(rule_type: &str, parameters: Option<serde_json::Value>) -> BranchRule {
        BranchRule {
            rule_type: rule_type.into(),
            parameters,
            ruleset_id: 1,
            ruleset_source: "acme/widget".into(),
            ruleset_source_type: "Repository".into(),
        }
    }

    fn required_status_checks_rule(contexts: &[(&str, u64)]) -> BranchRule {
        let params = serde_json::json!({
            "required_status_checks": contexts
                .iter()
                .map(|(c, id)| serde_json::json!({"context": c, "integration_id": id}))
                .collect::<Vec<_>>(),
        });
        branch_rule("required_status_checks", Some(params))
    }

    fn check(name: &str, state: crate::observe::github::checks::CheckState) -> PullRequestCheck {
        PullRequestCheck {
            name: crate::ids::CheckName::parse(name).unwrap(),
            state,
            description: String::new(),
            link: String::new(),
            completed_at: None,
        }
    }

    #[test]
    fn active_branch_rule_types_are_sorted_and_deduped() {
        let rules = vec![
            branch_rule("required_status_checks", None),
            branch_rule("required_signatures", None),
            branch_rule("required_status_checks", None),
            branch_rule("copilot_code_review", None),
        ];
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert_eq!(
            s.active_branch_rule_types,
            vec![
                "copilot_code_review",
                "required_signatures",
                "required_status_checks",
            ],
        );
    }

    #[test]
    fn active_branch_rule_types_empty_when_no_rules() {
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(s.active_branch_rule_types.is_empty());
        assert!(s.required_check_names_per_ruleset.is_empty());
        assert!(s.missing_required_check_names_on_head.is_empty());
    }

    #[test]
    fn required_check_names_per_ruleset_flattens_and_dedupes() {
        let rules = vec![
            required_status_checks_rule(&[("Build", 1), ("Lint", 1)]),
            required_status_checks_rule(&[("Lint", 1), ("Test", 2)]),
            branch_rule("required_signatures", None),
        ];
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert_eq!(
            s.required_check_names_per_ruleset,
            vec!["Build", "Lint", "Test"],
        );
    }

    #[test]
    fn required_status_checks_rule_with_unparseable_params_skipped() {
        let mut rule = required_status_checks_rule(&[("Build", 1)]);
        rule.parameters = Some(serde_json::json!({"unexpected": "shape"}));
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[rule], &[], None, vec![]);
        assert!(s.required_check_names_per_ruleset.is_empty());
    }

    #[test]
    fn missing_on_head_lists_required_checks_without_runs() {
        use crate::observe::github::checks::CheckState;
        let rules = vec![required_status_checks_rule(&[("Build", 1), ("Lint", 1)])];
        let head = vec![check("Build", CheckState::Success)];
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &head, None, vec![]);
        assert_eq!(s.missing_required_check_names_on_head, vec!["Lint"]);
    }

    fn protection_with(
        conv: Option<bool>,
        sigs: Option<bool>,
    ) -> crate::observe::github::branch_protection::BranchProtection {
        use crate::observe::github::branch_protection::{BranchProtection, EnabledFlag};
        BranchProtection {
            required_status_checks: None,
            required_conversation_resolution: conv.map(|enabled| EnabledFlag { enabled }),
            required_signatures: sigs.map(|enabled| EnabledFlag { enabled }),
        }
    }

    fn pull_request_rule(required_review_thread_resolution: bool) -> BranchRule {
        let params = serde_json::json!({
            "required_review_thread_resolution": required_review_thread_resolution,
        });
        branch_rule("pull_request", Some(params))
    }

    fn pull_request_rule_with_approving_count(n: u64) -> BranchRule {
        let params = serde_json::json!({
            "required_approving_review_count": n,
        });
        branch_rule("pull_request", Some(params))
    }

    #[test]
    fn required_approving_review_count_none_when_no_rule() {
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert_eq!(s.required_approving_review_count, None);
    }

    #[test]
    fn required_approving_review_count_extracted_from_ruleset() {
        let pr = pull_request_view(|_| {});
        let rules = vec![pull_request_rule_with_approving_count(1)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert_eq!(s.required_approving_review_count, Some(1));
    }

    #[test]
    fn required_approving_review_count_takes_maximum_when_multiple_rules() {
        let pr = pull_request_view(|_| {});
        let rules = vec![
            pull_request_rule_with_approving_count(1),
            pull_request_rule_with_approving_count(2),
        ];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert_eq!(s.required_approving_review_count, Some(2));
    }

    #[test]
    fn required_approving_review_count_zero_distinguished_from_absent() {
        let pr = pull_request_view(|_| {});
        let rules = vec![pull_request_rule_with_approving_count(0)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert_eq!(s.required_approving_review_count, Some(0));
    }

    #[test]
    fn copilot_review_required_off_when_no_rule() {
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.copilot_review_required);
    }

    #[test]
    fn copilot_review_required_on_when_ruleset_has_copilot_rule() {
        let pr = pull_request_view(|_| {});
        let rules = vec![branch_rule("copilot_code_review", None)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert!(s.copilot_review_required);
    }

    #[test]
    fn copilot_review_required_does_not_trigger_on_unrelated_rule() {
        let pr = pull_request_view(|_| {});
        let rules = vec![branch_rule("required_signatures", None)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert!(!s.copilot_review_required);
    }

    #[test]
    fn conversation_resolution_required_off_when_no_sources() {
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.conversation_resolution_required);
    }

    #[test]
    fn conversation_resolution_required_from_legacy_protection() {
        let pr = pull_request_view(|_| {});
        let p = protection_with(Some(true), None);
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], Some(&p), vec![]);
        assert!(s.conversation_resolution_required);
    }

    #[test]
    fn conversation_resolution_required_explicit_false_in_protection_reads_as_off() {
        let pr = pull_request_view(|_| {});
        let p = protection_with(Some(false), None);
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], Some(&p), vec![]);
        assert!(!s.conversation_resolution_required);
    }

    #[test]
    fn conversation_resolution_required_from_pull_request_ruleset() {
        let pr = pull_request_view(|_| {});
        let rules = vec![pull_request_rule(true)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert!(s.conversation_resolution_required);
    }

    #[test]
    fn conversation_resolution_required_pull_request_rule_param_false_reads_as_off() {
        let pr = pull_request_view(|_| {});
        let rules = vec![pull_request_rule(false)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert!(!s.conversation_resolution_required);
    }

    #[test]
    fn conversation_resolution_required_union_either_source() {
        // Either source on → on. (Union semantics.)
        let pr = pull_request_view(|_| {});
        let rules = vec![pull_request_rule(true)];
        let p = protection_with(Some(false), None);
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], Some(&p), vec![]);
        assert!(s.conversation_resolution_required);
    }

    #[test]
    fn signatures_required_off_when_no_sources() {
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], None, vec![]);
        assert!(!s.signatures_required);
    }

    #[test]
    fn signatures_required_from_legacy_protection() {
        let pr = pull_request_view(|_| {});
        let p = protection_with(None, Some(true));
        let s = orient_state(&pr, None, &pr.base_ref_name, &[], &[], Some(&p), vec![]);
        assert!(s.signatures_required);
    }

    #[test]
    fn signatures_required_from_ruleset_rule_type() {
        let pr = pull_request_view(|_| {});
        let rules = vec![branch_rule("required_signatures", None)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert!(s.signatures_required);
    }

    #[test]
    fn signatures_required_unrelated_rule_does_not_trigger() {
        let pr = pull_request_view(|_| {});
        let rules = vec![branch_rule("creation", None)];
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &[], None, vec![]);
        assert!(!s.signatures_required);
    }

    #[test]
    fn missing_on_head_ignores_check_conclusion() {
        use crate::observe::github::checks::CheckState;
        let rules = vec![required_status_checks_rule(&[("Build", 1), ("Lint", 1)])];
        let head = vec![
            check("Build", CheckState::Failure),
            check("Lint", CheckState::InProgress),
        ];
        let pr = pull_request_view(|_| {});
        let s = orient_state(&pr, None, &pr.base_ref_name, &rules, &head, None, vec![]);
        assert!(s.missing_required_check_names_on_head.is_empty());
    }
}
