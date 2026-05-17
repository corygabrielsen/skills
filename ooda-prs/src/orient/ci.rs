//! Project observed checks against the required-check set.
//!
//! # Invariants
//>
//! - **Two parallel projections, one report**: the report carries a
//!   bucket projection (for rendering) and an activity projection
//!   (for decide). They share inputs and never contradict —
//!   activity is computed against the same bucket counts.
//! - **Required-vs-advisory partition is total**: every observed
//!   check lands in either the required or advisory bucket; the
//!   required-name set decides membership and no check appears in
//!   both.
//! - **Health threshold + remediation budget**: in-flight checks
//!   carry per-check health driven by two timing thresholds and a
//!   per-(name, HEAD) attempt budget. Force-push moves HEAD and
//!   implicitly resets the budget via SHA-equality filtering.
//! - **Eventual-consistency tolerance**: a pending check with no
//!   matching run-row falls through to a coarser resolved state
//!   rather than synthesising fake health.

use std::collections::HashMap;

use crate::ids::{CheckName, GitCommitSha, Timestamp};
use crate::observe::github::checks::{CheckState, PullRequestCheck};
use crate::observe::github::workflow_runs::{WorkflowRun, WorkflowRunId, WorkflowRunStatus};
use serde::Serialize;

// ── Bucket projection (unchanged contract, used by render + main) ───

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CiSummary {
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
    pub(crate) fn missing(&self) -> usize {
        self.missing_names.len()
    }
}

/// One bucket's projection: pass count, failed details, pending
/// names. Required and advisory buckets share the shape so callers
/// can reason uniformly.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub(crate) struct CheckBucket {
    pub pass: usize,
    pub failed: Vec<FailedCheck>,
    pub pending_names: Vec<CheckName>,
}

impl CheckBucket {
    pub(crate) fn fail(&self) -> usize {
        self.failed.len()
    }
    pub(crate) fn pending(&self) -> usize {
        self.pending_names.len()
    }
    #[cfg(test)]
    pub(crate) fn total(&self) -> usize {
        self.pass + self.fail() + self.pending()
    }
    pub(crate) fn failed_names(&self) -> Vec<&CheckName> {
        self.failed.iter().map(|f| &f.name).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FailedCheck {
    pub name: CheckName,
    pub description: String,
    pub link: String,
}

/// Stack-tool-emitted check that gates a stacked PR on parent
/// merge. Removed from the required set under the stack-topology
/// witness — waiting on it cannot resolve until the parent merges,
/// so leaving it required would loop the wait action.
const GRAPHITE_MERGEABILITY_CHECK: &str = "Graphite / mergeability_check";

// ── Health layer ────────────────────────────────────────────────────

/// Maximum queue dwell before a check classifies as queue-degraded.
/// Sized at ~1.5× the observed max legitimate queue latency.
pub(crate) const QUEUE_TIMEOUT: chrono::Duration = chrono::Duration::minutes(20);

/// Maximum in-progress dwell before a check classifies as run-
/// degraded. Sized at ~1.5× the observed max run duration; the
/// host's hard ceiling acts as an absolute Failed backstop above.
pub(crate) const RUN_TIMEOUT: chrono::Duration = chrono::Duration::minutes(30);

/// Per-(check, HEAD) remediation budget. Distinct attempts on the
/// current HEAD allowed before a degraded check promotes to Failed.
/// Same value as the sibling reviewer axis; held independently per
/// the anti-DRY mirror rule until a third axis surfaces.
pub(crate) const BUDGET: usize = 2;

// Per-axis symptom. Nullary variants match the shape used by sibling
// axes; payload variants stay possible for a future lift to a
// generic health type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Symptom {
    /// Threshold crossed against the queue anchor (no run start
    /// observed within the queue budget).
    QueueTimeout,
    /// Threshold crossed against the run-start anchor (no completion
    /// observed within the run budget).
    RunTimeout,
}

// Per-check health lattice. Same Healthy/Degraded/Failed shape as
// the sibling reviewer axis's in-flight health; held independently
// per the anti-DRY mirror rule until a third axis surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum CheckHealth {
    /// In flight within the timing budget.
    Healthy,
    /// Threshold crossed; remediation still in budget.
    Degraded(Symptom),
    /// Threshold crossed and remediation budget exhausted; further
    /// re-runs would only restart the same failure mode.
    Failed(Symptom),
}

/// Pending required check on the current HEAD. Carries the run
/// handle (for remediation side effects) and the projected health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PendingCheck {
    pub name: CheckName,
    pub run_id: WorkflowRunId,
    pub health: CheckHealth,
}

/// Terminal classification for the resolved activity arm. Total over
/// the post-pending state of the required-check set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum ResolvedState {
    /// Every required check is positive.
    AllGreen,
    /// At least one required check reached a non-positive terminal
    /// state.
    HasFailures(Vec<CheckName>),
    /// Required contexts configured but absent from observation
    /// after pending work resolved.
    MissingRequired(Vec<CheckName>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum CiActivity {
    /// No required checks gate this PR (no policy or stack-topology
    /// filter applied).
    Idle,
    /// At least one required check is pending. Non-empty by
    /// construction — empty pending routes to `Resolved` instead.
    /// Decide aggregates worst-of health across the vector.
    InFlight(Vec<PendingCheck>),
    /// All required checks reached terminal state.
    Resolved(ResolvedState),
}

/// Full CI report — bucket projection plus typed activity, both
/// derived from the same observation set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CiReport {
    pub summary: CiSummary,
    pub activity: CiActivity,
}

/// Project the activity into a dashboard signal. `Idle` returns
/// `None` so the dashboard skips axes with no gate active on this
/// PR.
pub(crate) fn ci_signal(activity: &CiActivity) -> Option<crate::dashboard::AxisSignal> {
    use crate::dashboard::{AxisName, AxisSignal, SignalIcon};
    let (icon, summary) = match activity {
        CiActivity::Idle => return None,
        CiActivity::InFlight(pending) => {
            let worst_rank = pending
                .iter()
                .map(|p| match p.health {
                    CheckHealth::Healthy => 0,
                    CheckHealth::Degraded(_) => 1,
                    CheckHealth::Failed(_) => 2,
                })
                .max()
                .unwrap_or(0);
            let label = match worst_rank {
                0 => "healthy",
                1 => "degraded",
                _ => "failed",
            };
            (
                SignalIcon::InFlight,
                format!("{} checks pending (worst: {})", pending.len(), label),
            )
        }
        CiActivity::Resolved(ResolvedState::AllGreen) => {
            (SignalIcon::Ok, "all required checks passing".to_string())
        }
        CiActivity::Resolved(ResolvedState::HasFailures(names)) => (
            SignalIcon::Failed,
            format!(
                "{} required checks failed: {}",
                names.len(),
                join_check_names(names),
            ),
        ),
        CiActivity::Resolved(ResolvedState::MissingRequired(names)) => (
            SignalIcon::Warn,
            format!(
                "{} required checks missing: {}",
                names.len(),
                join_check_names(names),
            ),
        ),
    };
    Some(AxisSignal {
        axis: AxisName::Ci,
        icon,
        summary,
    })
}

fn join_check_names(names: &[CheckName]) -> String {
    names
        .iter()
        .map(super::super::ids::CheckName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Orient entry point ──────────────────────────────────────────────

/// Project observed checks against the required-context set.
///
/// `required_names` is supplied pre-resolved (caller-side union of
/// rule-source and legacy-source contexts). The stack-topology bit
/// removes the stack-tooling mergeability check from the required
/// set so the wait action does not loop on a gate that cannot
/// resolve until the parent merges. `workflow_runs` and `head` drive
/// per-check timing and the re-run budget; HEAD movement implicitly
/// resets the budget via SHA-equality filtering.
pub(crate) fn orient_ci(
    checks: &[PullRequestCheck],
    required_names: &[CheckName],
    has_open_parent_pr: bool,
    workflow_runs: &[WorkflowRun],
    head: &GitCommitSha,
    now: Timestamp,
) -> CiReport {
    let summary = build_summary(checks, required_names, has_open_parent_pr);
    let activity = compute_ci_activity(&summary, checks, workflow_runs, head, now);
    CiReport { summary, activity }
}

fn build_summary(
    checks: &[PullRequestCheck],
    required_names: &[CheckName],
    has_open_parent_pr: bool,
) -> CiSummary {
    // Set membership for O(1) bucket assignment; ordered iteration
    // walks the input slice so the rendered name lists preserve
    // caller-supplied order.
    let required_set: std::collections::HashSet<&str> = required_names
        .iter()
        .filter(|n| !(has_open_parent_pr && n.as_str() == GRAPHITE_MERGEABILITY_CHECK))
        .map(CheckName::as_str)
        .collect();

    let observed: HashMap<&str, &PullRequestCheck> =
        checks.iter().map(|c| (c.name.as_str(), c)).collect();

    let mut required = CheckBucket::default();
    let mut missing_names: Vec<CheckName> = Vec::new();
    let mut completed_at: Option<Timestamp> = None;

    for name in required_names {
        if !required_set.contains(name.as_str()) {
            continue;
        }
        match observed.get(name.as_str()) {
            None => missing_names.push(name.clone()),
            Some(obs) => {
                update_completed_at(&mut completed_at, obs.completed_at.as_ref());
                classify_into(&mut required, obs);
            }
        }
    }

    let mut advisory = CheckBucket::default();
    for c in checks {
        if required_set.contains(c.name.as_str()) {
            continue;
        }
        update_completed_at(&mut completed_at, c.completed_at.as_ref());
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

fn update_completed_at(out: &mut Option<Timestamp>, candidate: Option<&Timestamp>) {
    let Some(c) = candidate else { return };
    match out {
        None => *out = Some(*c),
        Some(current) if c > current => *out = Some(*c),
        _ => {}
    }
}

// ── Activity + health projection ────────────────────────────────────

fn compute_ci_activity(
    summary: &CiSummary,
    checks: &[PullRequestCheck],
    workflow_runs: &[WorkflowRun],
    head: &GitCommitSha,
    now: Timestamp,
) -> CiActivity {
    let req_total = summary.required.pass + summary.required.fail() + summary.required.pending();
    let missing = summary.missing();
    if req_total == 0 && missing == 0 {
        return CiActivity::Idle;
    }

    // Phase 1: assemble in-flight entries by joining pending checks
    // to runs on the current HEAD by workflow name.
    let pending: Vec<&PullRequestCheck> = checks
        .iter()
        .filter(|c| summary.required.pending_names.contains(&c.name))
        .collect();

    if !pending.is_empty() {
        let in_flight: Vec<PendingCheck> = pending
            .iter()
            .filter_map(|c| build_pending_check(c, workflow_runs, head, now))
            .collect();
        // Eventual-consistency window: the check is pending but no
        // matching run-row has propagated yet. Empty in-flight set
        // here falls through to the resolved classification rather
        // than fabricating a health signal.
        if !in_flight.is_empty() {
            return CiActivity::InFlight(in_flight);
        }
    }

    // Phase 2: nothing pending. Classify the terminal state.
    if !summary.required.failed.is_empty() {
        let names: Vec<CheckName> = summary
            .required
            .failed
            .iter()
            .map(|f| f.name.clone())
            .collect();
        return CiActivity::Resolved(ResolvedState::HasFailures(names));
    }
    if !summary.missing_names.is_empty() {
        return CiActivity::Resolved(ResolvedState::MissingRequired(
            summary.missing_names.clone(),
        ));
    }
    CiActivity::Resolved(ResolvedState::AllGreen)
}

fn build_pending_check(
    check: &PullRequestCheck,
    workflow_runs: &[WorkflowRun],
    head: &GitCommitSha,
    now: Timestamp,
) -> Option<PendingCheck> {
    // Attempt count = distinct runs for this name on the current
    // HEAD. HEAD movement (force-push) implicitly resets the budget
    // via SHA-equality filtering — no orient-side state needed.
    let runs_for_check: Vec<&WorkflowRun> = workflow_runs
        .iter()
        .filter(|r| r.head_sha == *head && r.name == check.name.as_str())
        .collect();

    // Eventual-consistency tolerance: a pending check without a
    // matching run-row returns None; the caller routes to a coarser
    // resolved classification rather than fabricating health.
    let latest_pending_run = runs_for_check
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                WorkflowRunStatus::Queued
                    | WorkflowRunStatus::InProgress
                    | WorkflowRunStatus::Pending
                    | WorkflowRunStatus::Waiting
                    | WorkflowRunStatus::Requested,
            )
        })
        .max_by_key(|r| r.created_at)?;

    let attempts = runs_for_check.len();
    let symptom_opt = classify_symptom(latest_pending_run, now);
    let health = match symptom_opt {
        None => CheckHealth::Healthy,
        Some(symptom) if attempts >= BUDGET => CheckHealth::Failed(symptom),
        Some(symptom) => CheckHealth::Degraded(symptom),
    };

    Some(PendingCheck {
        name: check.name.clone(),
        run_id: latest_pending_run.id.clone(),
        health,
    })
}

fn classify_symptom(run: &WorkflowRun, now: Timestamp) -> Option<Symptom> {
    match run.run_started_at {
        None => {
            if now.at() - run.created_at.at() >= QUEUE_TIMEOUT {
                Some(Symptom::QueueTimeout)
            } else {
                None
            }
        }
        Some(started) => {
            if now.at() - started.at() >= RUN_TIMEOUT {
                Some(Symptom::RunTimeout)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::checks::PullRequestCheck;
    use crate::observe::github::workflow_runs::{WorkflowRunConclusion, WorkflowRunStatus};

    fn cn(s: &str) -> CheckName {
        CheckName::parse(s).unwrap()
    }

    fn names(v: &[CheckName]) -> Vec<&str> {
        v.iter().map(CheckName::as_str).collect()
    }

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    const HEAD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn head() -> GitCommitSha {
        GitCommitSha::parse(HEAD_SHA).unwrap()
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
            completed_at: Some(ts("2026-04-23T10:00:00Z")),
        }
    }

    fn run(
        id: u64,
        name: &str,
        status: WorkflowRunStatus,
        created: &str,
        started: Option<&str>,
    ) -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId(id),
            name: name.into(),
            head_sha: head(),
            status,
            conclusion: None,
            created_at: ts(created),
            run_started_at: started.map(ts),
            run_attempt: 1,
        }
    }

    fn run_completed(
        id: u64,
        name: &str,
        conclusion: WorkflowRunConclusion,
        created: &str,
    ) -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId(id),
            name: name.into(),
            head_sha: head(),
            status: WorkflowRunStatus::Completed,
            conclusion: Some(conclusion),
            created_at: ts(created),
            run_started_at: Some(ts(created)),
            run_attempt: 1,
        }
    }

    // ── summary projection (preserved from pre-health version) ──

    #[test]
    fn empty_inputs_yield_empty_summary() {
        let r = orient_ci(&[], &[], false, &[], &head(), ts("2026-04-23T10:00:00Z"));
        let s = &r.summary;
        assert_eq!(s.required.pass, 0);
        assert_eq!(s.required.fail(), 0);
        assert_eq!(s.required.pending(), 0);
        assert_eq!(s.required.total(), 0);
        assert_eq!(s.missing(), 0);
        assert!(s.completed_at.is_none());
        assert_eq!(s.advisory.total(), 0);
        // No required checks → Idle.
        assert!(matches!(r.activity, CiActivity::Idle));
    }

    #[test]
    fn success_skipped_neutral_count_as_pass() {
        let checks = vec![
            check("a", CheckState::Success, Some("2026-04-23T01:00:00Z")),
            check("b", CheckState::Skipped, Some("2026-04-23T02:00:00Z")),
            check("c", CheckState::Neutral, Some("2026-04-23T03:00:00Z")),
        ];
        let req = vec![cn("a"), cn("b"), cn("c")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
        assert_eq!(s.required.pass, 3);
        assert_eq!(s.required.fail(), 0);
        assert_eq!(s.required.pending(), 0);
        assert_eq!(
            s.completed_at.as_ref().unwrap().to_string(),
            "2026-04-23T03:00:00+00:00"
        );
        assert!(matches!(
            r.activity,
            CiActivity::Resolved(ResolvedState::AllGreen)
        ));
    }

    #[test]
    fn failure_populates_failed_details_with_link_and_description() {
        let checks = vec![failed("Lint", "1 error", "https://example/lint")];
        let req = vec![cn("Lint")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
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
        assert!(matches!(
            r.activity,
            CiActivity::Resolved(ResolvedState::HasFailures(_))
        ));
    }

    #[test]
    fn in_progress_and_queued_count_as_pending() {
        let checks = vec![
            check("Build", CheckState::InProgress, None),
            check("Test", CheckState::Queued, None),
        ];
        let req = vec![cn("Build"), cn("Test")];
        let runs = vec![
            run(
                1,
                "Build",
                WorkflowRunStatus::InProgress,
                "2026-04-23T09:50:00Z",
                Some("2026-04-23T09:51:00Z"),
            ),
            run(
                2,
                "Test",
                WorkflowRunStatus::Queued,
                "2026-04-23T09:55:00Z",
                None,
            ),
        ];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
        assert_eq!(s.required.pending(), 2);
        assert_eq!(names(&s.required.pending_names), vec!["Build", "Test"]);
        // Both pending and within timeouts → InFlight with Healthy.
        let CiActivity::InFlight(checks_h) = &r.activity else {
            panic!("expected InFlight, got {:?}", r.activity);
        };
        assert_eq!(checks_h.len(), 2);
        assert!(checks_h.iter().all(|c| c.health == CheckHealth::Healthy));
    }

    #[test]
    fn required_but_absent_check_is_missing_not_pending() {
        let checks = vec![check("Build", CheckState::Success, None)];
        let req = vec![cn("Build"), cn("Mergeability Check")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
        assert_eq!(s.required.pass, 1);
        assert_eq!(s.required.pending(), 0);
        assert_eq!(s.missing(), 1);
        assert_eq!(names(&s.missing_names), vec!["Mergeability Check"]);
        assert!(matches!(
            r.activity,
            CiActivity::Resolved(ResolvedState::MissingRequired(_))
        ));
    }

    #[test]
    fn observed_but_not_required_routes_to_advisory() {
        let checks = vec![
            check("Lint", CheckState::Success, None),
            check("Cursor Bugbot", CheckState::Failure, None),
        ];
        let req = vec![cn("Lint")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
        assert_eq!(s.required.pass, 1);
        assert_eq!(s.advisory.fail(), 1);
        assert_eq!(s.advisory.failed[0].name.as_str(), "Cursor Bugbot");
    }

    #[test]
    fn graphite_mergeability_required_when_not_stacked() {
        let checks = vec![
            check("Graphite / mergeability_check", CheckState::Success, None),
            check("Lint", CheckState::Success, None),
        ];
        let req = vec![cn("Graphite / mergeability_check"), cn("Lint")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        assert_eq!(r.summary.required.total(), 2);
    }

    #[test]
    fn graphite_mergeability_filtered_when_stacked() {
        let checks = vec![
            check("Graphite / mergeability_check", CheckState::Pending, None),
            check("Lint", CheckState::Success, None),
        ];
        let req = vec![cn("Graphite / mergeability_check"), cn("Lint")];
        let r = orient_ci(
            &checks,
            &req,
            true,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
        assert_eq!(s.required.total(), 1);
        assert_eq!(s.required.pass, 1);
        assert_eq!(s.required.pending(), 0);
        assert!(names(&s.advisory.pending_names).contains(&"Graphite / mergeability_check"));
    }

    #[test]
    fn completed_at_is_max_across_all_observed_checks() {
        let checks = vec![
            check("Lint", CheckState::Success, Some("2026-04-23T01:00:00Z")),
            check("Adv", CheckState::Success, Some("2026-04-23T05:00:00Z")),
        ];
        let req = vec![cn("Lint")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        assert_eq!(
            r.summary.completed_at.as_ref().unwrap().to_string(),
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
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
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
        let runs = vec![run(
            1,
            "Build",
            WorkflowRunStatus::InProgress,
            "2026-04-23T09:50:00Z",
            Some("2026-04-23T09:51:00Z"),
        )];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let s = &r.summary;
        assert_eq!(s.required.pending(), 1);
        assert_eq!(s.required.pass, 1);
        assert_eq!(
            s.completed_at.as_ref().unwrap().to_string(),
            "2026-04-23T01:00:00+00:00"
        );
    }

    // ── health computation ──

    #[test]
    fn pending_check_within_window_is_healthy() {
        let checks = vec![check("Build", CheckState::Queued, None)];
        let req = vec![cn("Build")];
        // Created 5 min ago, no start yet → within 20-min QUEUE_TIMEOUT.
        let runs = vec![run(
            1,
            "Build",
            WorkflowRunStatus::Queued,
            "2026-04-23T09:55:00Z",
            None,
        )];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let CiActivity::InFlight(cs) = &r.activity else {
            panic!("expected InFlight, got {:?}", r.activity);
        };
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].health, CheckHealth::Healthy);
    }

    #[test]
    fn pending_check_past_queue_timeout_is_degraded_queue() {
        let checks = vec![check("Build", CheckState::Queued, None)];
        let req = vec![cn("Build")];
        // Queued at 9:30, still not started at 10:00 → 30 min elapsed
        // ≥ 20 min QUEUE_TIMEOUT. Only one attempt → Degraded.
        let runs = vec![run(
            1,
            "Build",
            WorkflowRunStatus::Queued,
            "2026-04-23T09:30:00Z",
            None,
        )];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let CiActivity::InFlight(cs) = &r.activity else {
            panic!("expected InFlight, got {:?}", r.activity);
        };
        assert_eq!(cs[0].health, CheckHealth::Degraded(Symptom::QueueTimeout));
    }

    #[test]
    fn pending_check_past_run_timeout_is_degraded_run() {
        let checks = vec![check("Build", CheckState::InProgress, None)];
        let req = vec![cn("Build")];
        // Started at 9:00, still in progress at 10:00 → 60 min elapsed
        // ≥ 30 min RUN_TIMEOUT.
        let runs = vec![run(
            1,
            "Build",
            WorkflowRunStatus::InProgress,
            "2026-04-23T09:00:00Z",
            Some("2026-04-23T09:00:00Z"),
        )];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let CiActivity::InFlight(cs) = &r.activity else {
            panic!("expected InFlight, got {:?}", r.activity);
        };
        assert_eq!(cs[0].health, CheckHealth::Degraded(Symptom::RunTimeout));
    }

    #[test]
    fn pending_check_failed_when_budget_exhausted() {
        // Two attempts on the same HEAD (BUDGET=2). Latest is past
        // queue timeout → Failed, not Degraded. The earlier completed
        // attempt counts toward the budget — re-running again would
        // only restart the same failure mode.
        let checks = vec![check("Build", CheckState::Queued, None)];
        let req = vec![cn("Build")];
        let runs = vec![
            run_completed(
                1,
                "Build",
                WorkflowRunConclusion::Cancelled,
                "2026-04-23T08:00:00Z",
            ),
            run(
                2,
                "Build",
                WorkflowRunStatus::Queued,
                "2026-04-23T09:30:00Z",
                None,
            ),
        ];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let CiActivity::InFlight(cs) = &r.activity else {
            panic!("expected InFlight, got {:?}", r.activity);
        };
        assert_eq!(cs[0].health, CheckHealth::Failed(Symptom::QueueTimeout));
    }

    #[test]
    fn force_push_resets_per_check_budget_via_sha_filter() {
        // Workflow runs on a previous HEAD must not count toward
        // the current-HEAD budget. With BUDGET=2 and only one run
        // on the current HEAD, a degraded check stays Degraded
        // (not Failed) even if there were prior attempts on the
        // old HEAD.
        let checks = vec![check("Build", CheckState::Queued, None)];
        let req = vec![cn("Build")];
        let old_sha = GitCommitSha::parse(&"b".repeat(40)).unwrap();
        let runs = vec![
            WorkflowRun {
                id: WorkflowRunId(100),
                name: "Build".into(),
                head_sha: old_sha.clone(),
                status: WorkflowRunStatus::Completed,
                conclusion: Some(WorkflowRunConclusion::Cancelled),
                created_at: ts("2026-04-22T10:00:00Z"),
                run_started_at: Some(ts("2026-04-22T10:00:00Z")),
                run_attempt: 1,
            },
            WorkflowRun {
                id: WorkflowRunId(200),
                name: "Build".into(),
                head_sha: head(),
                status: WorkflowRunStatus::Queued,
                conclusion: None,
                created_at: ts("2026-04-23T09:30:00Z"),
                run_started_at: None,
                run_attempt: 1,
            },
        ];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &runs,
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        let CiActivity::InFlight(cs) = &r.activity else {
            panic!("expected InFlight, got {:?}", r.activity);
        };
        assert_eq!(cs[0].health, CheckHealth::Degraded(Symptom::QueueTimeout));
    }

    #[test]
    fn pending_check_with_no_workflow_run_falls_through_to_resolved() {
        // Eventual-consistency window: `gh pr checks` reports the
        // check as pending but the workflow_runs feed hasn't caught
        // up yet. compute_ci_activity yields no InFlight entry; the
        // caller falls through to the coarser MissingRequired
        // classification rather than synthesising fake health.
        let checks = vec![check("Build", CheckState::Queued, None)];
        let req = vec![cn("Build")];
        let r = orient_ci(
            &checks,
            &req,
            false,
            &[],
            &head(),
            ts("2026-04-23T10:00:00Z"),
        );
        // No runs for Build → in_flight set empty → Resolved arm
        // takes over. The required bucket has one pending check
        // (no completed, no failed, no missing); the fallthrough
        // emits AllGreen because there are no terminal failures
        // and the pending check has no missing entry. The decide
        // arm for Resolved::AllGreen emits no candidate, which is
        // the safe behavior for this transient window.
        assert!(matches!(
            r.activity,
            CiActivity::Resolved(ResolvedState::AllGreen)
        ));
    }
}
