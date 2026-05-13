//! CI orient: project observed check-runs against the configured
//! required-check set, producing the `CiSummary` decide consumes.
//!
//! Scope of this module is intentionally narrow — produce facts only.
//! Higher-level concepts (Findings, Opportunities, blockers) compose
//! over this in later modules once a second axis lands and forces the
//! shared abstraction.

use std::collections::HashMap;

use crate::ids::{CheckName, Timestamp};
use crate::observe::github::checks::{CheckState, PullRequestCheck};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CiSummary {
    /// Required-check counts (gating merge).
    pub required: CheckBucket,
    /// Required checks configured but not present on the PR yet.
    pub missing_names: Vec<CheckName>,
    /// Most recent completion time across all observed checks.
    pub completed_at: Option<Timestamp>,
    /// Non-required checks — surfaced for visibility, not gating.
    pub advisory: CheckBucket,
}

impl CiSummary {
    pub fn missing(&self) -> usize {
        self.missing_names.len()
    }
}

/// Counts + names + failed-detail tuples for one bucket of checks
/// (required *or* advisory). Same shape on both sides so callers can
/// reason uniformly.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct CheckBucket {
    pub pass: usize,
    pub failed: Vec<FailedCheck>,
    pub pending_names: Vec<CheckName>,
}

impl CheckBucket {
    pub fn fail(&self) -> usize {
        self.failed.len()
    }
    pub fn pending(&self) -> usize {
        self.pending_names.len()
    }
    pub fn total(&self) -> usize {
        self.pass + self.fail() + self.pending()
    }
    pub fn failed_names(&self) -> Vec<&CheckName> {
        self.failed.iter().map(|f| &f.name).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedCheck {
    pub name: CheckName,
    pub description: String,
    pub link: String,
}

/// Orient observed checks against the required-name set.
///
/// `required_names` is the union of branch-rules required-status-checks
/// and legacy branch-protection contexts (assembled by the caller).
/// Graphite's mergeability check is filtered from both sides — it's
/// not a CI signal.
pub fn orient_ci(checks: &[PullRequestCheck], required_names: &[CheckName]) -> CiSummary {
    // HashSet for O(1) advisory partitioning; order-bearing iteration
    // walks the input slice so pending_names / missing_names preserve
    // the caller's order. Graphite mergeability is treated as any
    // other required check — stack-blocked PRs surface it as
    // pending/failing CI rather than being silently dropped.
    let required_set: std::collections::HashSet<&str> =
        required_names.iter().map(CheckName::as_str).collect();

    let observed: HashMap<&str, &PullRequestCheck> =
        checks.iter().map(|c| (c.name.as_str(), c)).collect();

    let mut required = CheckBucket::default();
    let mut missing_names: Vec<CheckName> = Vec::new();
    let mut completed_at: Option<Timestamp> = None;

    for name in required_names {
        match observed.get(name.as_str()) {
            None => missing_names.push(name.clone()),
            Some(obs) => {
                update_completed_at(&mut completed_at, &obs.completed_at);
                classify_into(&mut required, obs);
            }
        }
    }

    let mut advisory = CheckBucket::default();
    for c in checks {
        if required_set.contains(c.name.as_str()) {
            continue;
        }
        update_completed_at(&mut completed_at, &c.completed_at);
        classify_into(&mut advisory, c);
    }

    CiSummary {
        required,
        missing_names,
        completed_at,
        advisory,
    }
}

fn classify_into(bucket: &mut CheckBucket, c: &PullRequestCheck) {
    match c.state {
        CheckState::Success | CheckState::Skipped | CheckState::Neutral => {
            bucket.pass += 1;
        }
        CheckState::Failure
        | CheckState::Cancelled
        | CheckState::TimedOut
        | CheckState::ActionRequired
        | CheckState::Stale
        | CheckState::StartupFailure
        | CheckState::Unknown => bucket.failed.push(FailedCheck {
            name: c.name.clone(),
            description: c.description.clone(),
            link: c.link.clone(),
        }),
        CheckState::InProgress | CheckState::Queued | CheckState::Pending => {
            bucket.pending_names.push(c.name.clone());
        }
    }
}

fn update_completed_at(out: &mut Option<Timestamp>, candidate: &Option<Timestamp>) {
    let Some(c) = candidate else { return };
    match out {
        None => *out = Some(*c),
        Some(current) if c > current => *out = Some(*c),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::checks::PullRequestCheck;

    fn cn(s: &str) -> CheckName {
        CheckName::parse(s).unwrap()
    }

    fn names(v: &[CheckName]) -> Vec<&str> {
        v.iter().map(CheckName::as_str).collect()
    }

    fn check(name: &str, state: CheckState, completed: Option<&str>) -> PullRequestCheck {
        PullRequestCheck {
            name: cn(name),
            state,
            description: String::new(),
            link: String::new(),
            completed_at: completed.map(|s| Timestamp::parse(s).unwrap()),
        }
    }

    fn failed(name: &str, desc: &str, link: &str) -> PullRequestCheck {
        PullRequestCheck {
            name: cn(name),
            state: CheckState::Failure,
            description: desc.to_owned(),
            link: link.to_owned(),
            completed_at: Some(Timestamp::parse("2026-04-23T10:00:00Z").unwrap()),
        }
    }

    #[test]
    fn empty_inputs_yield_empty_summary() {
        let s = orient_ci(&[], &[]);
        assert_eq!(s.required.pass, 0);
        assert_eq!(s.required.fail(), 0);
        assert_eq!(s.required.pending(), 0);
        assert_eq!(s.required.total(), 0);
        assert_eq!(s.missing(), 0);
        assert!(s.completed_at.is_none());
        assert_eq!(s.advisory.total(), 0);
    }

    #[test]
    fn success_skipped_neutral_count_as_pass() {
        let checks = vec![
            check("a", CheckState::Success, Some("2026-04-23T01:00:00Z")),
            check("b", CheckState::Skipped, Some("2026-04-23T02:00:00Z")),
            check("c", CheckState::Neutral, Some("2026-04-23T03:00:00Z")),
        ];
        let req = vec![cn("a"), cn("b"), cn("c")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.pass, 3);
        assert_eq!(s.required.fail(), 0);
        assert_eq!(s.required.pending(), 0);
        assert_eq!(
            s.completed_at.as_ref().unwrap().to_string(),
            "2026-04-23T03:00:00+00:00"
        );
    }

    #[test]
    fn failure_populates_failed_details_with_link_and_description() {
        let checks = vec![failed("Lint", "1 error", "https://example/lint")];
        let req = vec![cn("Lint")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.fail(), 1);
        assert_eq!(s.required.failed[0].name.as_str(), "Lint");
        assert_eq!(s.required.failed[0].description, "1 error");
        assert_eq!(s.required.failed[0].link, "https://example/lint");
        assert_eq!(
            s.required
                .failed_names()
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>(),
            vec!["Lint"],
        );
    }

    #[test]
    fn in_progress_and_queued_count_as_pending() {
        let checks = vec![
            check("Build", CheckState::InProgress, None),
            check("Test", CheckState::Queued, None),
        ];
        let req = vec![cn("Build"), cn("Test")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.pending(), 2);
        assert_eq!(names(&s.required.pending_names), vec!["Build", "Test"]);
    }

    #[test]
    fn required_but_absent_check_is_missing_not_pending() {
        let checks = vec![check("Build", CheckState::Success, None)];
        let req = vec![cn("Build"), cn("Mergeability Check")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.pass, 1);
        assert_eq!(s.required.pending(), 0);
        assert_eq!(s.missing(), 1);
        assert_eq!(names(&s.missing_names), vec!["Mergeability Check"]);
    }

    #[test]
    fn observed_but_not_required_routes_to_advisory() {
        let checks = vec![
            check("Lint", CheckState::Success, None),
            check("Cursor Bugbot", CheckState::Failure, None),
        ];
        let req = vec![cn("Lint")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.pass, 1);
        assert_eq!(s.advisory.fail(), 1);
        assert_eq!(s.advisory.failed[0].name.as_str(), "Cursor Bugbot");
    }

    #[test]
    fn graphite_mergeability_treated_as_normal_required_check() {
        // Stack-blocked PRs surface this check as pending/failure
        // — orient must NOT silently filter it, or Halt::Success
        // can fire on a non-mergeable stacked PR.
        let checks = vec![
            check("Graphite / mergeability_check", CheckState::Success, None),
            check("Lint", CheckState::Success, None),
        ];
        let req = vec![cn("Graphite / mergeability_check"), cn("Lint")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.total(), 2);
    }

    #[test]
    fn completed_at_is_max_across_all_observed_checks() {
        // Required and advisory both contribute to completed_at.
        let checks = vec![
            check("Lint", CheckState::Success, Some("2026-04-23T01:00:00Z")),
            check("Adv", CheckState::Success, Some("2026-04-23T05:00:00Z")),
        ];
        let req = vec![cn("Lint")];
        let s = orient_ci(&checks, &req);
        assert_eq!(
            s.completed_at.as_ref().unwrap().to_string(),
            "2026-04-23T05:00:00+00:00"
        );
    }

    #[test]
    fn missing_check_does_not_advance_completed_at() {
        let checks = vec![check(
            "Lint",
            CheckState::Success,
            Some("2026-04-23T01:00:00Z"),
        )];
        let req = vec![cn("Lint"), cn("Build")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.missing(), 1);
        assert_eq!(
            s.completed_at.as_ref().unwrap().to_string(),
            "2026-04-23T01:00:00+00:00"
        );
    }

    #[test]
    fn pending_check_with_no_completed_at_does_not_set_completed_at() {
        let checks = vec![
            check("Build", CheckState::InProgress, None),
            check("Lint", CheckState::Success, Some("2026-04-23T01:00:00Z")),
        ];
        let req = vec![cn("Build"), cn("Lint")];
        let s = orient_ci(&checks, &req);
        assert_eq!(s.required.pending(), 1);
        assert_eq!(s.required.pass, 1);
        assert_eq!(
            s.completed_at.as_ref().unwrap().to_string(),
            "2026-04-23T01:00:00+00:00"
        );
    }
}
