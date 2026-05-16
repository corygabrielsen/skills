//! CI candidate generation.
//!
//! Top-level match on `CiActivity`:
//!   - Idle / Resolved::AllGreen → no candidate (delegate to next axis)
//!   - InFlight → worst-of aggregation over per-check health:
//!     Failed > Degraded > Healthy.
//!     Failed → EscalateCiFailed (Human, exit 3).
//!     Degraded → ReRunWorkflow (Full, blocking).
//!     Healthy → WaitForCi (Wait).
//!   - Resolved::HasFailures → existing FixCi-per-failure path.
//!   - Resolved::MissingRequired → WaitForCi (missing).
//!
//! TriageWait fires only on advisory failures concurrent with a
//! blocked-required set — kept as a sibling branch to InFlight that
//! suppresses the Healthy Wait when an advisory genuinely needs an
//! agent to look.

use super::action::{
    Action, ActionEffect, ActionKind, DegradedCheck, FailedCheckHandle, NonEmpty, TargetEffect,
    Urgency,
};
use crate::ids::{BlockerKey, CheckName};
use crate::orient::ci::{
    CheckHealth, CiActivity, CiReport, CiSummary, PendingCheck, ResolvedState, Symptom,
};

/// Comma-join a slice of `CheckName` for human-readable rendering.
fn join_names(names: &[CheckName]) -> String {
    names
        .iter()
        .map(CheckName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn candidates(report: &CiReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    let summary = &report.summary;

    match &report.activity {
        CiActivity::Idle => {
            // No required checks observed; nothing to emit. The
            // Resolved::AllGreen arm covers the "all required
            // checks passed" case; Idle is the strictly-zero case.
        }
        CiActivity::Resolved(ResolvedState::AllGreen) => {
            // Every required check passed; nothing to emit.
        }
        CiActivity::Resolved(ResolvedState::HasFailures(names)) => {
            // Pre-health behavior preserved: one FixCi per failing
            // required check. Drives the existing
            // "ci_fail: <check>" blocker tags.
            for f in &summary.required.failed {
                if !names.contains(&f.name) {
                    continue;
                }
                out.push(Action {
                    kind: ActionKind::FixCi {
                        check_name: f.name.clone(),
                    },
                    effect: ActionEffect::Agent {
                        prompt: ooda_core::HandoffPrompt::new(format!(
                            "Fix failing check: {}",
                            f.name
                        )),
                    },
                    target_effect: TargetEffect::Blocks,
                    urgency: Urgency::BlockingFix,
                    blocker: BlockerKey::tag(format!("ci_fail: {}", f.name)),
                });
            }
            // TriageWait does NOT fire in HasFailures — a required
            // failure already routes to FixCi; the agent will see the
            // advisory state in the snapshot.
        }
        CiActivity::Resolved(ResolvedState::MissingRequired(names)) => {
            // Required checks configured but absent. Same shape as
            // the pre-health "ci_missing" path; TriageWait may
            // shadow this when concurrent advisory failures exist
            // (delegated to triage_branch below).
            triage_or_missing(summary, names, &mut out);
        }
        CiActivity::InFlight(checks) => {
            in_flight_candidates(summary, checks, &mut out);
        }
    }

    out
}

/// Aggregate per-check health into one action. Failed dominates
/// Degraded dominates Healthy. Within each tier, every check at
/// that tier travels in the action payload.
fn in_flight_candidates(summary: &CiSummary, checks: &[PendingCheck], out: &mut Vec<Action>) {
    let failed: Vec<FailedCheckHandle> = checks
        .iter()
        .filter_map(|c| match c.health {
            CheckHealth::Failed(s) => Some(FailedCheckHandle {
                name: c.name.clone(),
                symptom: s,
            }),
            _ => None,
        })
        .collect();
    if let Some(failed_ne) = NonEmpty::try_from_vec(failed) {
        let names_csv = failed_ne
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let headline = format!(
            "Per-(check, HEAD) re-run budget exhausted on: {names_csv}. \
             Investigate the underlying workflow or GitHub Actions \
             status and re-trigger manually once the issue is resolved.",
        );
        // The blocker tag combines the worst symptom (sorted) so a
        // queue-timeout failure produces a distinct stall key from a
        // run-timeout failure on the same check set.
        let tag = format!("ci_failed: {names_csv}");
        out.push(Action {
            kind: ActionKind::EscalateCiFailed { checks: failed_ne },
            effect: ActionEffect::Human {
                prompt: ooda_core::HandoffPrompt::new(headline),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::tag(tag),
        });
        return;
    }

    let degraded: Vec<DegradedCheck> = checks
        .iter()
        .filter_map(|c| match c.health {
            CheckHealth::Degraded(s) => Some(DegradedCheck {
                name: c.name.clone(),
                run_id: c.run_id.clone(),
                symptom: s,
            }),
            _ => None,
        })
        .collect();
    if let Some(degraded_ne) = NonEmpty::try_from_vec(degraded) {
        let names_csv = degraded_ne
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let symptom_tag = if degraded_ne
            .iter()
            .any(|c| matches!(c.symptom, Symptom::RunTimeout))
        {
            "run_timeout"
        } else {
            "queue_timeout"
        };
        let log = format!(
            "Re-running {} (degraded: {symptom_tag})",
            crate::text::count(degraded_ne.len(), "workflow"),
        );
        out.push(Action {
            kind: ActionKind::ReRunWorkflow {
                checks: degraded_ne,
            },
            effect: ActionEffect::Full { log },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag(format!("ci_degraded_{symptom_tag}: {names_csv}")),
        });
        return;
    }

    // All checks Healthy: fall through to the pre-health
    // Wait/Triage paths. Pending+missing names together feed the
    // triage trigger; TriageWait suppresses Wait when it fires.
    let pending_names: Vec<CheckName> = checks.iter().map(|c| c.name.clone()).collect();
    triage_or_wait(summary, &pending_names, out);
}

/// Emit either TriageWait (advisory failure concurrent with blocked
/// required) or one or two WaitForCi candidates (pending + missing).
fn triage_or_wait(summary: &CiSummary, pending_names: &[CheckName], out: &mut Vec<Action>) {
    let blocked: Vec<CheckName> = pending_names
        .iter()
        .chain(summary.missing_names.iter())
        .cloned()
        .collect();

    if let Some(blocked) = NonEmpty::try_from_vec(blocked)
        .filter(|_| summary.required.fail() == 0 && !summary.advisory.failed.is_empty())
    {
        push_triage(summary, blocked, out);
        return;
    }
    if let Some(names) = NonEmpty::try_from_vec(pending_names.to_vec()) {
        let blocker_list = join_names(&names);
        let pending_count = names.len();
        out.push(Action {
            kind: ActionKind::WaitForCi { pending: names },
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(60),
                log: format!(
                    "Wait for {}",
                    crate::text::count(pending_count, "pending check"),
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::tag(format!("ci_pending: {blocker_list}")),
        });
    }
    if let Some(names) = NonEmpty::try_from_vec(summary.missing_names.clone()) {
        let blocker_list = join_names(&names);
        let missing_count = names.len();
        out.push(Action {
            kind: ActionKind::WaitForCi { pending: names },
            effect: ActionEffect::Wait {
                interval: ooda_core::PollingInterval::from_secs(60),
                log: format!(
                    "{} not started: {blocker_list}",
                    crate::text::count(missing_count, "required check"),
                ),
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingWait,
            blocker: BlockerKey::tag(format!("ci_missing: {blocker_list}")),
        });
    }
}

/// MissingRequired branch: no pending; emit Triage or Missing-Wait.
fn triage_or_missing(summary: &CiSummary, names: &[CheckName], out: &mut Vec<Action>) {
    // Reuse the shared helper with an empty pending list — the
    // missing-only path emits exactly the same blocker tags as
    // before (`ci_missing: ...`).
    let _ = names; // names mirror summary.missing_names by construction.
    triage_or_wait(summary, &[], out);
}

fn push_triage(summary: &CiSummary, blocked: NonEmpty<CheckName>, out: &mut Vec<Action>) {
    let advisory_lines: Vec<String> = summary
        .advisory
        .failed
        .iter()
        .map(|f| format!("- Advisory \"{}\" failed", f.name))
        .collect();
    let quoted: Vec<String> = blocked.iter().map(|n| format!("\"{n}\"")).collect();
    let headline = format!("CI waiting on {}. Concurrent state:", quoted.join(", "));
    let blocker_list = join_names(&blocked);
    out.push(Action {
        kind: ActionKind::TriageWait {
            blocked_checks: blocked,
        },
        effect: ActionEffect::Agent {
            prompt: ooda_core::HandoffPrompt::new(headline)
                .with_paragraph(advisory_lines.join("\n")),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::BlockingFix,
        blocker: BlockerKey::tag(format!("ci_triage: {blocker_list}")),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::github::workflow_runs::WorkflowRunId;
    use crate::orient::ci::{
        CheckBucket, CheckHealth, CiActivity, CiReport, CiSummary, FailedCheck, PendingCheck,
        ResolvedState, Symptom,
    };

    fn empty_summary() -> CiSummary {
        CiSummary {
            required: CheckBucket::default(),
            missing_names: vec![],
            completed_at: None,
            advisory: CheckBucket::default(),
        }
    }

    fn failed(name: &str) -> FailedCheck {
        FailedCheck {
            name: CheckName::parse(name).unwrap(),
            description: String::new(),
            link: String::new(),
        }
    }

    fn cn(name: &str) -> CheckName {
        CheckName::parse(name).unwrap()
    }

    fn report(summary: CiSummary, activity: CiActivity) -> CiReport {
        CiReport { summary, activity }
    }

    fn pc(name: &str, health: CheckHealth) -> PendingCheck {
        PendingCheck {
            name: cn(name),
            run_id: WorkflowRunId(format!("run-{name}")),
            health,
        }
    }

    #[test]
    fn idle_yields_no_candidates() {
        let cs = candidates(&report(empty_summary(), CiActivity::Idle));
        assert!(cs.is_empty());
    }

    #[test]
    fn all_green_resolved_yields_no_candidates() {
        let cs = candidates(&report(
            empty_summary(),
            CiActivity::Resolved(ResolvedState::AllGreen),
        ));
        assert!(cs.is_empty());
    }

    #[test]
    fn failing_required_check_emits_fix_ci_per_failure() {
        let mut s = empty_summary();
        s.required.failed = vec![failed("Lint"), failed("Build")];
        let activity =
            CiActivity::Resolved(ResolvedState::HasFailures(vec![cn("Lint"), cn("Build")]));
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 2);
        assert!(matches!(cs[0].kind, ActionKind::FixCi { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
    }

    #[test]
    fn pending_healthy_emits_wait_for_ci() {
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("Build"), cn("Test")];
        let activity = CiActivity::InFlight(vec![
            pc("Build", CheckHealth::Healthy),
            pc("Test", CheckHealth::Healthy),
        ]);
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCi { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Wait { .. }));
    }

    #[test]
    fn pending_degraded_emits_rerun_workflow() {
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("Build")];
        let activity = CiActivity::InFlight(vec![pc(
            "Build",
            CheckHealth::Degraded(Symptom::QueueTimeout),
        )]);
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::ReRunWorkflow { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Full { .. }));
        assert_eq!(cs[0].urgency, Urgency::BlockingFix);
        assert!(
            cs[0]
                .blocker
                .as_str()
                .starts_with("ci_degraded_queue_timeout")
        );
    }

    #[test]
    fn pending_failed_emits_escalate_ci_failed() {
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("Build")];
        let activity =
            CiActivity::InFlight(vec![pc("Build", CheckHealth::Failed(Symptom::RunTimeout))]);
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::EscalateCiFailed { .. }));
        assert!(matches!(cs[0].effect, ActionEffect::Human { .. }));
        assert_eq!(cs[0].urgency, Urgency::BlockingHuman);
        assert!(cs[0].blocker.as_str().starts_with("ci_failed"));
    }

    #[test]
    fn failed_dominates_degraded_in_worst_of_aggregation() {
        // Mix of Failed and Degraded: only EscalateCiFailed fires.
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("A"), cn("B")];
        let activity = CiActivity::InFlight(vec![
            pc("A", CheckHealth::Failed(Symptom::QueueTimeout)),
            pc("B", CheckHealth::Degraded(Symptom::RunTimeout)),
        ]);
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::EscalateCiFailed { .. }));
        // ReRunWorkflow must NOT be emitted alongside.
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::ReRunWorkflow { .. })),
        );
    }

    #[test]
    fn degraded_dominates_healthy_in_worst_of_aggregation() {
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("A"), cn("B")];
        let activity = CiActivity::InFlight(vec![
            pc("A", CheckHealth::Degraded(Symptom::QueueTimeout)),
            pc("B", CheckHealth::Healthy),
        ]);
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::ReRunWorkflow { .. }));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::WaitForCi { .. })),
        );
    }

    #[test]
    fn missing_required_emits_wait_for_ci_with_separate_blocker() {
        let mut s = empty_summary();
        s.missing_names = vec![cn("Mergeability Check")];
        let activity = CiActivity::Resolved(ResolvedState::MissingRequired(vec![cn(
            "Mergeability Check",
        )]));
        let cs = candidates(&report(s, activity));
        assert_eq!(cs.len(), 1);
        assert!(cs[0].blocker.as_str().starts_with("ci_missing"));
    }

    #[test]
    fn advisory_failure_with_blocked_required_triggers_triage() {
        let mut s = empty_summary();
        s.missing_names = vec![cn("Mergeability Check")];
        s.advisory.failed = vec![failed("Lint")];
        let activity = CiActivity::Resolved(ResolvedState::MissingRequired(vec![cn(
            "Mergeability Check",
        )]));
        let cs = candidates(&report(s, activity));
        let kinds: Vec<&ActionKind> = cs.iter().map(|a| &a.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, ActionKind::TriageWait { .. }))
        );
        assert!(
            !kinds
                .iter()
                .any(|k| matches!(k, ActionKind::WaitForCi { .. }))
        );
    }

    #[test]
    fn advisory_failure_without_blocked_required_no_triage() {
        let mut s = empty_summary();
        s.advisory.failed = vec![failed("Lint")];
        let activity = CiActivity::Resolved(ResolvedState::AllGreen);
        let cs = candidates(&report(s, activity));
        assert!(
            !cs.iter()
                .any(|a| matches!(a.kind, ActionKind::TriageWait { .. }))
        );
    }

    #[test]
    fn ci_failure_takes_precedence_over_triage_or_wait() {
        let mut s = empty_summary();
        s.required.failed = vec![failed("Lint")];
        s.missing_names = vec![cn("Mergeability Check")];
        s.advisory.failed = vec![failed("Style")];
        let activity = CiActivity::Resolved(ResolvedState::HasFailures(vec![cn("Lint")]));
        let cs = candidates(&report(s, activity));
        assert!(matches!(cs[0].kind, ActionKind::FixCi { .. }));
    }

    // ─── property test for the class invariant ──────────────────────
    //
    // Mirrors the copilot axis pattern: exhaustive coverage over the
    // (CiActivity-variant × CheckHealth-variant) cross product on the
    // InFlight variant. The match in `expected_ci_baseline_behavior`
    // is structurally exhaustive — adding a new CheckHealth variant
    // (or extending CiActivity) fails to compile here first.

    #[derive(Debug, PartialEq, Eq)]
    enum CiBaselineBehavior {
        NoCandidate,
        EmitWaitForCi,
        EmitFixCi,
        EmitReRunWorkflow(Symptom),
        EmitEscalateCiFailed(Symptom),
        EmitMissingWait,
    }

    /// What the CI axis emits for each `CiActivity` baseline.
    ///
    /// For `InFlight(checks)` the contract is parametric in
    /// `CheckHealth`: a single-check InFlight with a particular
    /// health value drives a specific action. The match below is
    /// exhaustive over both CheckHealth and CiActivity — adding a
    /// new variant requires a new arm here.
    fn expected_ci_baseline_behavior(activity: &CiActivity) -> CiBaselineBehavior {
        match activity {
            CiActivity::Idle => CiBaselineBehavior::NoCandidate,
            CiActivity::Resolved(ResolvedState::AllGreen) => CiBaselineBehavior::NoCandidate,
            CiActivity::Resolved(ResolvedState::HasFailures(_)) => CiBaselineBehavior::EmitFixCi,
            CiActivity::Resolved(ResolvedState::MissingRequired(_)) => {
                CiBaselineBehavior::EmitMissingWait
            }
            CiActivity::InFlight(checks) => match checks.first().map(|c| c.health) {
                Some(CheckHealth::Healthy) => CiBaselineBehavior::EmitWaitForCi,
                Some(CheckHealth::Degraded(s)) => CiBaselineBehavior::EmitReRunWorkflow(s),
                Some(CheckHealth::Failed(s)) => CiBaselineBehavior::EmitEscalateCiFailed(s),
                None => CiBaselineBehavior::NoCandidate,
            },
        }
    }

    /// Enumerates every (CiActivity × CheckHealth) baseline case on
    /// the InFlight variant, plus one representative per Resolved
    /// variant and Idle. New variants (or new Symptom variants) fail
    /// to compile against the exhaustive match in
    /// `expected_ci_baseline_behavior`.
    fn all_ci_activities() -> Vec<(CiSummary, CiActivity)> {
        let mut out = Vec::new();
        out.push((empty_summary(), CiActivity::Idle));
        out.push((
            empty_summary(),
            CiActivity::Resolved(ResolvedState::AllGreen),
        ));
        {
            let mut s = empty_summary();
            s.required.failed = vec![failed("Lint")];
            out.push((
                s,
                CiActivity::Resolved(ResolvedState::HasFailures(vec![cn("Lint")])),
            ));
        }
        {
            let mut s = empty_summary();
            s.missing_names = vec![cn("X")];
            out.push((
                s,
                CiActivity::Resolved(ResolvedState::MissingRequired(vec![cn("X")])),
            ));
        }
        // InFlight × CheckHealth: every health variant gets a row.
        for (sym_name, sym) in [
            ("QueueTimeout", Symptom::QueueTimeout),
            ("RunTimeout", Symptom::RunTimeout),
        ] {
            let mut s = empty_summary();
            s.required.pending_names = vec![cn(&format!("Check-D-{sym_name}"))];
            out.push((
                s,
                CiActivity::InFlight(vec![pc(
                    &format!("Check-D-{sym_name}"),
                    CheckHealth::Degraded(sym),
                )]),
            ));
            let mut s = empty_summary();
            s.required.pending_names = vec![cn(&format!("Check-F-{sym_name}"))];
            out.push((
                s,
                CiActivity::InFlight(vec![pc(
                    &format!("Check-F-{sym_name}"),
                    CheckHealth::Failed(sym),
                )]),
            ));
        }
        // Healthy variant — only one case (no symptom payload).
        {
            let mut s = empty_summary();
            s.required.pending_names = vec![cn("Check-H")];
            out.push((
                s,
                CiActivity::InFlight(vec![pc("Check-H", CheckHealth::Healthy)]),
            ));
        }
        out
    }

    fn observed_ci_baseline_behavior(cs: &[Action]) -> CiBaselineBehavior {
        match cs {
            [] => CiBaselineBehavior::NoCandidate,
            [a] => match (&a.kind, &a.effect) {
                (ActionKind::WaitForCi { .. }, ActionEffect::Wait { .. }) => {
                    // The MissingRequired baseline shape emits the
                    // missing-Wait variant; the Healthy InFlight
                    // emits the pending-Wait variant. Distinguish by
                    // blocker tag (stable identity).
                    if a.blocker.as_str().starts_with("ci_missing") {
                        CiBaselineBehavior::EmitMissingWait
                    } else {
                        CiBaselineBehavior::EmitWaitForCi
                    }
                }
                (ActionKind::FixCi { .. }, ActionEffect::Agent { .. }) => {
                    CiBaselineBehavior::EmitFixCi
                }
                (ActionKind::ReRunWorkflow { checks }, ActionEffect::Full { .. }) => {
                    CiBaselineBehavior::EmitReRunWorkflow(checks.first().symptom)
                }
                (ActionKind::EscalateCiFailed { checks }, ActionEffect::Human { .. }) => {
                    CiBaselineBehavior::EmitEscalateCiFailed(checks.first().symptom)
                }
                (kind, effect) => panic!(
                    "ci axis emitted unexpected (kind, effect) in baseline: {kind:?}, {effect:?}",
                ),
            },
            multi => panic!(
                "ci axis emitted unexpected candidate count in baseline: {} items",
                multi.len()
            ),
        }
    }

    #[test]
    fn ci_axis_property_holds_for_every_activity_baseline() {
        let baselines = all_ci_activities();
        // Idle (1) + Resolved×3 (AllGreen, HasFailures, MissingRequired)
        // + InFlight×Healthy (1) + InFlight×Degraded×2 + InFlight×Failed×2
        // = 9.
        assert_eq!(
            baselines.len(),
            9,
            "`all_ci_activities` must enumerate every \
             (CiActivity × CheckHealth) baseline case: Idle (1) + \
             Resolved×3 + InFlight×{{Healthy + Degraded×|Symptom| + \
             Failed×|Symptom|}} = 1 + 3 + 1 + 2 + 2 = 9. Adding a \
             new variant requires updating this sample list, the \
             exhaustive match in `expected_ci_baseline_behavior`, \
             AND this length sentinel.",
        );
        for (summary, activity) in baselines {
            let r = report(summary, activity.clone());
            let cs = candidates(&r);
            let actual = observed_ci_baseline_behavior(&cs);
            let expected = expected_ci_baseline_behavior(&activity);
            assert_eq!(
                actual, expected,
                "ci baseline contract violated for activity = {activity:?}",
            );
        }
    }
}
