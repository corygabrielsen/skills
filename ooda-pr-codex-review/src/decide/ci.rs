//! CI-axis candidate generation.
//!
//! Two top-level shapes feed the candidate set: a `Resolved` state
//! (every required check has settled at HEAD) and an `InFlight`
//! state (one or more required checks still running). The `InFlight`
//! arm aggregates per-check health by *worst-of* — Failed dominates
//! Degraded dominates Healthy — and emits exactly one candidate at
//! the dominant tier.
//!
//! Advisory failures concurrent with a blocked required set
//! trigger a triage candidate that suppresses the otherwise-fired
//! Healthy Wait: an advisory failure on its own is not a blocker,
//! but co-occurring with a blocked required set it is genuinely
//! ambiguous and needs agent triage.

use super::action::{
    Action, ActionEffect, ActionKind, DegradedCheck, FailedCheckHandle, NonEmpty, TargetEffect,
    Urgency,
};
use crate::ids::{BlockerKey, CheckName};
use crate::orient::ci::{
    CheckHealth, CiActivity, CiReport, CiSummary, FailedCheck, PendingCheck, ResolvedState, Symptom,
};
use ooda_core::{HandoffPrompt, SingleLineString, Witness};

/// Comma-join a slice of `CheckName` for human-readable rendering.
fn join_names(names: &[CheckName]) -> String {
    names
        .iter()
        .map(CheckName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn candidates(report: &CiReport) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    let summary = &report.summary;

    // Exhaustive over CI activity; Idle and AllGreen are kept as
    // distinct arms for spec clarity even though both are empty.
    #[allow(clippy::match_same_arms)]
    match &report.activity {
        CiActivity::Idle => {
            // No required checks configured.
        }
        CiActivity::Resolved(ResolvedState::AllGreen) => {
            // All required checks passed.
        }
        CiActivity::Resolved(ResolvedState::HasFailures(names)) => {
            // One per-check fix candidate per failing required
            // check, keyed by check identity for stall stability.
            for f in &summary.required.failed {
                if !names.contains(&f.name) {
                    continue;
                }
                out.push(Action {
                    kind: ActionKind::FixCi {
                        check_name: f.name.clone(),
                    },
                    effect: ActionEffect::Agent {
                        prompt: fix_ci_prompt(f),
                    },
                    target_effect: TargetEffect::Blocks,
                    urgency: Urgency::BlockingFix,
                    blocker: BlockerKey::typed("ci_fail", &f.name),
                });
            }
            // Triage does not fire in HasFailures — the per-check
            // fix candidates already cover the work, and the
            // agent sees advisory state in the same snapshot.
        }
        CiActivity::Resolved(ResolvedState::MissingRequired(names)) => {
            // Required checks configured but absent at HEAD.
            // Triage may shadow this when an advisory failure
            // co-occurs; the helper routes either way.
            triage_or_missing(summary, names, &mut out);
        }
        CiActivity::InFlight(checks) => {
            in_flight_candidates(summary, checks, &mut out);
        }
    }

    out
}

/// Aggregate per-check health into one candidate at the dominant
/// tier (Failed > Degraded > Healthy). Every check at the chosen
/// tier travels in the action payload; lower-tier checks are
/// implicitly covered by the chosen action's gate.
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
        let prompt = escalate_ci_failed_prompt(&failed_ne, summary, &names_csv);
        out.push(Action {
            kind: ActionKind::EscalateCiFailed { checks: failed_ne },
            effect: ActionEffect::Human { prompt },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            // Gate identity: "≥1 required check failed". Per-check
            // names travel on the payload — embedding them in the
            // key would violate gate stability across iterations.
            blocker: BlockerKey::from_static("ci_failed"),
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
            // Gate identity: the (rerun-loop, symptom-class) pair.
            // Cohort identities travel on the payload. Each branch
            // resolves to a `&'static str` so the key is type-
            // witnessed at the call site.
            blocker: match symptom_tag {
                "run_timeout" => BlockerKey::from_static("ci_degraded_run_timeout"),
                "queue_timeout" => BlockerKey::from_static("ci_degraded_queue_timeout"),
                _ => unreachable!("symptom_tag is statically one of the two literals above"),
            },
        });
        return;
    }

    // Healthy fall-through: pending and missing names jointly
    // feed the triage / wait decision; triage suppresses the wait
    // when it fires.
    let pending_names: Vec<CheckName> = checks.iter().map(|c| c.name.clone()).collect();
    triage_or_wait(summary, &pending_names, out);
}

/// Emit either a triage candidate (advisory failure concurrent
/// with a blocked required set) or up to two wait candidates
/// (pending, missing). The two paths are mutually exclusive by
/// construction.
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
            // Gate identity: "≥1 required check pending". Cohort
            // travels on the payload.
            blocker: BlockerKey::from_static("ci_pending"),
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
            // Gate identity: "≥1 required check missing". Cohort
            // travels on the payload.
            blocker: BlockerKey::from_static("ci_missing"),
        });
    }
}

/// Missing-required branch: no pending names, so the shared helper
/// runs with an empty pending list and the same gate identities
/// fall out.
fn triage_or_missing(summary: &CiSummary, names: &[CheckName], out: &mut Vec<Action>) {
    let _ = names; // Identical to `summary.missing_names` by construction.
    triage_or_wait(summary, &[], out);
}

fn push_triage(summary: &CiSummary, blocked: NonEmpty<CheckName>, out: &mut Vec<Action>) {
    let quoted: Vec<String> = blocked.iter().map(|n| format!("\"{n}\"")).collect();
    let headline = format!(
        "CI waiting on {}. {} advisory failed concurrently.",
        quoted.join(", "),
        crate::text::count(summary.advisory.failed.len(), "advisory check"),
    );
    let prompt = triage_wait_prompt(headline, &summary.advisory.failed);
    out.push(Action {
        kind: ActionKind::TriageWait {
            blocked_checks: blocked,
        },
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::BlockingFix,
        // Gate identity: the concurrent (required-blocked,
        // advisory-failed) condition. Agent effects participate
        // in stall detection, so the key must be cohort-stable.
        blocker: BlockerKey::from_static("ci_triage"),
    });
}

/// One witness per failed check. Description and link inline as
/// body lines when present; a placeholder preserves the witness
/// shape when neither is available. Reused across the CI prompts
/// so their rendered shape stays uniform.
fn failed_check_witness(f: &FailedCheck) -> Witness {
    let label = SingleLineString::new(f.name.as_str().to_string());
    let mut body_lines: Vec<String> = Vec::new();
    if !f.description.trim().is_empty() {
        body_lines.push(format!("  {}", f.description.trim()));
    }
    if !f.link.trim().is_empty() {
        body_lines.push(format!("  Run: {}", f.link.trim()));
    }
    if body_lines.is_empty() {
        // The label still names the check; the placeholder
        // preserves the per-check witness shape.
        body_lines.push("  (no run details available)".to_string());
    }
    let url = if f.link.trim().is_empty() {
        None
    } else {
        Some(f.link.trim().to_string())
    };
    Witness {
        label,
        body: body_lines.join("\n"),
        url,
    }
}

fn fix_ci_prompt(f: &FailedCheck) -> HandoffPrompt {
    let mut prompt = HandoffPrompt::new(format!("Fix failing check: {}", f.name));
    let description = f.description.trim();
    let link = f.link.trim();
    if !description.is_empty() {
        prompt.push_paragraph(format!("Description: {description}"));
    }
    if !link.is_empty() {
        prompt.push_paragraph(format!("Run: {link}"));
    }
    prompt
}

fn escalate_ci_failed_prompt(
    failed_ne: &NonEmpty<FailedCheckHandle>,
    summary: &CiSummary,
    names_csv: &str,
) -> HandoffPrompt {
    let headline = format!(
        "Per-(check, HEAD) re-run budget exhausted on: {names_csv}. \
         Investigate the underlying workflow or GitHub Actions \
         status and re-trigger manually once the issue is resolved.",
    );
    let mut prompt = HandoffPrompt::new(headline);

    // One witness per failed handle. The matched summary entry
    // supplies description and link; when the upstream feeds are
    // mid-propagation and no match exists, the witness still
    // names the check and symptom so per-check structure is
    // preserved.
    let witnesses_vec: Vec<Witness> = failed_ne
        .iter()
        .map(|handle| {
            let symptom_label = symptom_slug(handle.symptom);
            let label = SingleLineString::new(format!("{} [{symptom_label}]", handle.name));
            let matched = summary
                .required
                .failed
                .iter()
                .find(|f| f.name == handle.name);
            let (body, url) = match matched {
                Some(f) => {
                    let w = failed_check_witness(f);
                    (w.body, w.url)
                }
                None => (
                    "  (no description / link available — eventual-consistency window)".to_string(),
                    None,
                ),
            };
            Witness { label, body, url }
        })
        .collect();
    if let Some(ws) = NonEmpty::try_from_vec(witnesses_vec) {
        prompt.push_witnesses(ws);
    }
    prompt
}

fn triage_wait_prompt(headline: String, advisory_failed: &[FailedCheck]) -> HandoffPrompt {
    let mut prompt = HandoffPrompt::new(headline);
    if let Some(ws) =
        NonEmpty::try_from_vec(advisory_failed.iter().map(failed_check_witness).collect())
    {
        prompt.push_paragraph("Advisory failures (non-blocking, surfaced for triage):");
        prompt.push_witnesses(ws);
    }
    prompt
}

fn symptom_slug(s: Symptom) -> &'static str {
    match s {
        Symptom::QueueTimeout => "queue-timeout",
        Symptom::RunTimeout => "run-timeout",
    }
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
            // Deterministic id-from-name keeps test fixtures
            // self-documenting; the hash is reproducible.
            run_id: WorkflowRunId(stable_id(name)),
            health,
        }
    }

    fn stable_id(name: &str) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        h.finish()
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
        // Worst-of: Failed dominates a co-occurring Degraded; only
        // the escalation candidate fires.
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

    // ─── per-variant baseline property ────────────────────────────
    //
    // Pins the class invariant: every (Activity × CheckHealth)
    // baseline maps to a determined emission. The exhaustive
    // match below is the contract; a new variant in either enum
    // fails to compile until handled.

    #[derive(Debug, PartialEq, Eq)]
    enum CiBaselineBehavior {
        NoCandidate,
        EmitWaitForCi,
        EmitFixCi,
        EmitReRunWorkflow(Symptom),
        EmitEscalateCiFailed(Symptom),
        EmitMissingWait,
    }

    /// Baseline projection over CI activity. The `InFlight` arm
    /// parameterises further on the first check's health, so the
    /// contract is exhaustive over both enums simultaneously.
    fn expected_ci_baseline_behavior(activity: &CiActivity) -> CiBaselineBehavior {
        // Arms duplicated for spec clarity.
        #[allow(clippy::match_same_arms)]
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

    /// Sample enumeration: every (`Activity` × `CheckHealth`) baseline
    /// on the `InFlight` arm, plus one representative for each
    /// `Resolved` arm and Idle. New variants fall through to the
    /// exhaustive match above and fail to compile until handled.
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
                    // Two distinct gates share the WaitForCi
                    // action kind; gate identity (blocker key)
                    // is what separates them.
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
        // Length sentinel: 1 + 3 + (1 + 2 + 2) = 9 across the
        // activity / health cross-product.
        assert_eq!(
            baselines.len(),
            9,
            "Sample enumeration must cover Idle + every Resolved \
             variant + every (InFlight × CheckHealth) case.",
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

    // ── prompt-enrichment tests ─────────────────────────────────────

    fn failed_with(name: &str, desc: &str, link: &str) -> FailedCheck {
        FailedCheck {
            name: CheckName::parse(name).unwrap(),
            description: desc.into(),
            link: link.into(),
        }
    }

    #[test]
    fn fix_ci_prompt_inlines_description_and_link() {
        let mut s = empty_summary();
        s.required.failed = vec![failed_with(
            "Lint",
            "3 errors in src/foo.rs",
            "https://github.com/o/r/actions/runs/123",
        )];
        let cs = candidates(&report(
            s,
            CiActivity::Resolved(ResolvedState::HasFailures(vec![cn("Lint")])),
        ));
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Fix failing check: Lint"));
        assert!(
            rendered.contains("Description: 3 errors in src/foo.rs"),
            "missing description: {rendered}",
        );
        assert!(
            rendered.contains("Run: https://github.com/o/r/actions/runs/123"),
            "missing link: {rendered}",
        );
    }

    #[test]
    fn fix_ci_prompt_omits_paragraphs_when_observation_lacks_them() {
        // Headline survives a missing description/link; no empty
        // detail paragraphs are emitted.
        let mut s = empty_summary();
        s.required.failed = vec![failed("Lint")];
        let cs = candidates(&report(
            s,
            CiActivity::Resolved(ResolvedState::HasFailures(vec![cn("Lint")])),
        ));
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Fix failing check: Lint"));
        assert!(!rendered.contains("Description:"));
        assert!(!rendered.contains("Run:"));
    }

    #[test]
    fn escalate_ci_failed_emits_witness_per_check_with_description_and_link() {
        // Two pending Failed checks at this HEAD; the matching
        // summary.required.failed entries carry the run details.
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("Build"), cn("Test")];
        s.required.failed = vec![
            failed_with(
                "Build",
                "compile error in src/foo.rs",
                "https://example/build",
            ),
            failed_with("Test", "1 test failing: it_works", "https://example/test"),
        ];
        let activity = CiActivity::InFlight(vec![
            pc("Build", CheckHealth::Failed(Symptom::RunTimeout)),
            pc("Test", CheckHealth::Failed(Symptom::QueueTimeout)),
        ]);
        let cs = candidates(&report(s, activity));
        let rendered = cs[0].rendered_payload();
        assert!(
            rendered.contains("Build [run-timeout]"),
            "Build label: {rendered}"
        );
        assert!(rendered.contains("compile error in src/foo.rs"));
        assert!(rendered.contains("Run: https://example/build"));
        assert!(rendered.contains("Test [queue-timeout]"));
        assert!(rendered.contains("1 test failing: it_works"));
        assert!(rendered.contains("Run: https://example/test"));
    }

    #[test]
    fn escalate_ci_failed_emits_witness_with_fallback_when_summary_lacks_match() {
        // Mid-propagation between upstream feeds: handle exists,
        // matching summary entry does not. Witness still names
        // the check and symptom and notes the gap.
        let mut s = empty_summary();
        s.required.pending_names = vec![cn("Build")];
        // summary.required.failed intentionally empty.
        let activity =
            CiActivity::InFlight(vec![pc("Build", CheckHealth::Failed(Symptom::RunTimeout))]);
        let cs = candidates(&report(s, activity));
        let rendered = cs[0].rendered_payload();
        assert!(rendered.contains("Build [run-timeout]"));
        assert!(rendered.contains("eventual-consistency window"));
    }

    #[test]
    fn triage_wait_prompt_emits_witness_per_advisory_failure() {
        let mut s = empty_summary();
        s.missing_names = vec![cn("Mergeability Check")];
        s.advisory.failed = vec![
            failed_with(
                "Lint",
                "8 warnings on master",
                "https://example/lint-advisory",
            ),
            failed_with("Style", "0 deltas", "https://example/style"),
        ];
        let activity = CiActivity::Resolved(ResolvedState::MissingRequired(vec![cn(
            "Mergeability Check",
        )]));
        let cs = candidates(&report(s, activity));
        let action = cs
            .iter()
            .find(|a| matches!(a.kind, ActionKind::TriageWait { .. }))
            .expect("TriageWait must fire");
        let rendered = action.rendered_payload();
        assert!(rendered.contains("CI waiting on \"Mergeability Check\""));
        assert!(
            rendered.contains("Advisory failures"),
            "missing advisory section: {rendered}",
        );
        assert!(rendered.contains("Lint"));
        assert!(rendered.contains("8 warnings on master"));
        assert!(rendered.contains("https://example/lint-advisory"));
        assert!(rendered.contains("Style"));
        assert!(rendered.contains("0 deltas"));
    }
}
