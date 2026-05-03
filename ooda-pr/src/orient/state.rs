//! State orient: project PR metadata into the typed state axis.
//!
//! Pure projection over a single observation source (`PullRequestView`)
//! — no joins, no cross-references. The simplest axis possible.

use crate::ids::Timestamp;
use crate::observe::github::pr_view::{MergeStateStatus, Mergeable, PullRequestView};

const TITLE_MAX_LEN: usize = 50;
/// The label string both orient (detects) and act (removes) must
/// agree on. Public so act/* can reference exactly the same value.
pub const WIP_LABEL: &str = "work in progress";
const MERGE_WHEN_READY_LABEL: &str = "merge-when-ready";
const CONTENT_LABELS: &[&str] = &["bug", "enhancement"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestState {
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
    /// `true` when the base branch has advanced past the merge base
    /// (i.e. PR needs rebasing for stack hygiene).
    pub behind: bool,
    /// Full merge state from GitHub. Preserved (not collapsed to
    /// `behind`) so decide can surface unmodeled merge blockers
    /// like deployment protection or custom rulesets — without
    /// this, a non-Clean status with otherwise-clean axes would
    /// halt as Success even though GitHub still blocks merge.
    pub merge_state_status: MergeStateStatus,
    pub updated_at: Timestamp,
    /// HEAD commit author timestamp. None when unavailable.
    pub last_commit_at: Option<Timestamp>,
}

/// Orient the PR-view observation into the state axis.
///
/// `last_commit_at` comes from a separate observation source (git
/// log on HEAD); the caller supplies it. Passed in rather than derived
/// here because it crosses an observation boundary.
pub fn orient_state(
    pr: &PullRequestView,
    last_commit_at: Option<Timestamp>,
) -> PullRequestState {
    let body = pr.body.as_deref().unwrap_or_default();
    let label_names: Vec<&str> = pr.labels.iter().map(|l| l.name.as_str()).collect();

    // Graphite appends a " (#NNN)" suffix to commit subjects on submit.
    // The 50-char title rule budgets for that auto-appendage.
    let suffix_len = format!(" (#{})", pr.number).len();
    let title_len = pr.title.chars().count() + suffix_len;

    PullRequestState {
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
        merge_state_status: pr.merge_state_status,
        updated_at: pr.updated_at,
        last_commit_at,
    }
}

/// True iff the body has a line starting with `## <heading>` (case
/// insensitive on the heading text only — `##` itself must be at line
/// start). Mirrors pr-fitness's regex `/^## <heading>/im`.
fn has_section_heading(body: &str, heading: &str) -> bool {
    let needle = format!("## {}", heading.to_ascii_lowercase());
    body.lines()
        .any(|line| line.to_ascii_lowercase().starts_with(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{GitCommitSha, GitHubLogin, PullRequestNumber};
    use crate::observe::github::pr_view::{
        Assignee, Commit, Label, PrState, PullRequestView, ReviewRequest,
    };

    fn pr_view(overrides: impl FnOnce(&mut PullRequestView)) -> PullRequestView {
        let mut v = PullRequestView {
            title: "fix thing".into(),
            number: PullRequestNumber::new(123).unwrap(),
            url: "https://example/pr/123".into(),
            body: Some("body".into()),
            state: PrState::Open,
            is_draft: false,
            mergeable: Mergeable::Mergeable,
            merge_state_status: MergeStateStatus::Clean,
            head_ref_oid: GitCommitSha::parse(&"a".repeat(40)).unwrap(),
            base_ref_name: crate::ids::BranchName::parse("master").unwrap(),
            updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
            closed_at: None,
            merged_at: None,
            review_decision: None,
            labels: vec![],
            assignees: vec![],
            review_requests: vec![],
            commits: vec![],
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
    fn defaults_for_minimal_open_pr() {
        let pr = pr_view(|_| {});
        let s = orient_state(&pr, None);
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
    fn title_len_includes_phantom_pr_suffix() {
        // "fix thing" = 9, " (#123)" = 7 → 16
        let pr = pr_view(|p| p.title = "fix thing".into());
        let s = orient_state(&pr, None);
        assert_eq!(s.title_len, 16);
        assert!(s.title_ok);
    }

    #[test]
    fn title_just_over_50_with_suffix_fails() {
        // Need title_len > 50. Suffix " (#123)" is 7 chars, so a
        // 44-char title gives total 51.
        let pr = pr_view(|p| p.title = "a".repeat(44));
        let s = orient_state(&pr, None);
        assert_eq!(s.title_len, 51);
        assert!(!s.title_ok);
    }

    #[test]
    fn title_at_exactly_50_passes() {
        let pr = pr_view(|p| p.title = "a".repeat(43));
        let s = orient_state(&pr, None);
        assert_eq!(s.title_len, 50);
        assert!(s.title_ok);
    }

    #[test]
    fn empty_body_marks_body_false_and_no_sections() {
        let pr = pr_view(|p| p.body = Some(String::new()));
        let s = orient_state(&pr, None);
        assert!(!s.body);
        assert!(!s.summary);
        assert!(!s.test_plan);
    }

    #[test]
    fn null_body_treated_as_empty() {
        let pr = pr_view(|p| p.body = None);
        let s = orient_state(&pr, None);
        assert!(!s.body);
    }

    #[test]
    fn detects_summary_and_test_plan_headings_case_insensitive() {
        let pr = pr_view(|p| {
            p.body = Some(
                "## summary\nstuff\n\n## TEST PLAN\n- check it\n".into(),
            );
        });
        let s = orient_state(&pr, None);
        assert!(s.summary);
        assert!(s.test_plan);
    }

    #[test]
    fn heading_must_be_at_line_start() {
        // Inline mention shouldn't trigger the heading detector.
        let pr = pr_view(|p| {
            p.body = Some("intro about ## summary in prose".into());
        });
        let s = orient_state(&pr, None);
        assert!(!s.summary);
    }

    #[test]
    fn wip_label_detected_exact_match() {
        let pr = pr_view(|p| p.labels.push(label("work in progress")));
        let s = orient_state(&pr, None);
        assert!(s.wip);
    }

    #[test]
    fn merge_when_ready_label_detected() {
        let pr = pr_view(|p| p.labels.push(label("merge-when-ready")));
        let s = orient_state(&pr, None);
        assert!(s.merge_when_ready);
    }

    #[test]
    fn content_label_recognizes_bug_or_enhancement() {
        for ct in ["bug", "enhancement"] {
            let pr = pr_view(|p| p.labels.push(label(ct)));
            let s = orient_state(&pr, None);
            assert!(s.content_label, "expected content_label for {ct}");
        }
        // A non-content label doesn't count.
        let pr = pr_view(|p| p.labels.push(label("documentation")));
        let s = orient_state(&pr, None);
        assert!(!s.content_label);
    }

    #[test]
    fn behind_only_when_merge_state_status_is_behind() {
        let pr = pr_view(|p| p.merge_state_status = MergeStateStatus::Behind);
        let s = orient_state(&pr, None);
        assert!(s.behind);

        let pr = pr_view(|p| p.merge_state_status = MergeStateStatus::Blocked);
        let s = orient_state(&pr, None);
        assert!(!s.behind);
    }

    #[test]
    fn counts_assignees_reviewers_and_commits() {
        let pr = pr_view(|p| {
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
                },
                Commit {
                    oid: GitCommitSha::parse(&"b".repeat(40)).unwrap(),
                },
            ];
        });
        let s = orient_state(&pr, None);
        assert_eq!(s.assignees, 1);
        assert_eq!(s.reviewers, 1);
        assert_eq!(s.commits, 2);
    }

    #[test]
    fn last_commit_at_passes_through() {
        let ts = Timestamp::parse("2026-04-23T11:00:00Z").unwrap();
        let pr = pr_view(|_| {});
        let s = orient_state(&pr, Some(ts));
        assert_eq!(s.last_commit_at, Some(ts));
    }
}
