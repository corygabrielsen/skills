//! CI candidate generation.
//!
//! Order:
//!   1. fix_ci for each failing required check (agent, blocks)
//!   2. triage_wait when CI is waiting on a fan-in AND an advisory
//!      check has actually failed (agent — genuinely ambiguous)
//!   3. wait_for_ci for pending or missing required checks (wait)
//!
//! Note (per design conversation): triage_wait fires ONLY on
//! advisory failures. Cursor-reviewing or Copilot-stale don't
//! qualify — those have concrete advancement actions in their
//! own axis candidate generators.

use std::time::Duration;

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};
use crate::ids::{BlockerKey, CheckName};
use crate::orient::ci::CiSummary;

/// Comma-join a slice of `CheckName` for human-readable rendering.
fn join_names(names: &[CheckName]) -> String {
    names
        .iter()
        .map(CheckName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn candidates(ci: &CiSummary) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();

    for f in &ci.required.failed {
        out.push(Action {
            kind: ActionKind::FixCi {
                check_name: f.name.clone(),
            },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: format!("Fix failing check: {}", f.name),
            blocker: BlockerKey::tag(format!("ci_fail: {}", f.name)),
        });
    }

    let blocked: Vec<CheckName> = ci
        .required
        .pending_names
        .iter()
        .chain(ci.missing_names.iter())
        .cloned()
        .collect();

    if ci.required.fail() == 0
        && !blocked.is_empty()
        && !ci.advisory.failed.is_empty()
    {
        let advisory_lines: Vec<String> = ci
            .advisory
            .failed
            .iter()
            .map(|f| format!("- Advisory \"{}\" failed", f.name))
            .collect();
        let quoted: Vec<String> =
            blocked.iter().map(|n| format!("\"{n}\"")).collect();
        let mut desc = vec![format!(
            "CI waiting on {}. Concurrent state:",
            quoted.join(", ")
        )];
        desc.extend(advisory_lines);
        let blocker_list = join_names(&blocked);
        out.push(Action {
            kind: ActionKind::TriageWait {
                blocked_checks: blocked,
            },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: desc.join("\n"),
            blocker: BlockerKey::tag(format!("ci_triage: {blocker_list}")),
        });
    } else {
        if !ci.required.pending_names.is_empty() {
            let names = ci.required.pending_names.clone();
            let blocker_list = join_names(&names);
            out.push(Action {
                kind: ActionKind::WaitForCi { pending: names },
                automation: Automation::Wait { interval: Duration::from_secs(60) },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                description: format!(
                    "Wait for {} pending check(s)",
                    ci.required.pending(),
                ),
                blocker: BlockerKey::tag(format!("ci_pending: {blocker_list}")),
            });
        }
        if !ci.missing_names.is_empty() {
            let names = ci.missing_names.clone();
            let blocker_list = join_names(&names);
            out.push(Action {
                kind: ActionKind::WaitForCi { pending: names },
                automation: Automation::Wait { interval: Duration::from_secs(60) },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                description: format!(
                    "{} required check(s) not started: {blocker_list}",
                    ci.missing(),
                ),
                blocker: BlockerKey::tag(format!("ci_missing: {blocker_list}")),
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orient::ci::{CheckBucket, CiSummary, FailedCheck};

    fn empty_ci() -> CiSummary {
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

    #[test]
    fn empty_ci_yields_no_candidates() {
        let cs = candidates(&empty_ci());
        assert!(cs.is_empty());
    }

    #[test]
    fn failing_required_check_emits_fix_ci_per_failure() {
        let mut ci = empty_ci();
        ci.required.failed = vec![failed("Lint"), failed("Build")];
        let cs = candidates(&ci);
        assert_eq!(cs.len(), 2);
        assert!(matches!(cs[0].kind, ActionKind::FixCi { .. }));
        assert_eq!(cs[0].automation, Automation::Agent);
    }

    #[test]
    fn pending_required_emits_wait_for_ci() {
        let mut ci = empty_ci();
        ci.required.pending_names = vec![cn("Build"), cn("Test")];
        let cs = candidates(&ci);
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCi { .. }));
        assert!(matches!(cs[0].automation, Automation::Wait { .. }));
    }

    #[test]
    fn missing_required_emits_wait_for_ci_with_separate_blocker() {
        let mut ci = empty_ci();
        ci.missing_names = vec![cn("Mergeability Check")];
        let cs = candidates(&ci);
        assert_eq!(cs.len(), 1);
        assert!(cs[0].blocker.as_str().starts_with("ci_missing"));
    }

    #[test]
    fn advisory_failure_with_blocked_required_triggers_triage() {
        let mut ci = empty_ci();
        ci.missing_names = vec![cn("Mergeability Check")];
        ci.advisory.failed = vec![failed("Lint")];
        let cs = candidates(&ci);
        let kinds: Vec<&ActionKind> = cs.iter().map(|a| &a.kind).collect();
        assert!(kinds
            .iter()
            .any(|k| matches!(k, ActionKind::TriageWait { .. })));
        // wait_for_ci suppressed when triage fires.
        assert!(!kinds
            .iter()
            .any(|k| matches!(k, ActionKind::WaitForCi { .. })));
    }

    #[test]
    fn advisory_failure_without_blocked_required_no_triage() {
        let mut ci = empty_ci();
        ci.advisory.failed = vec![failed("Lint")];
        let cs = candidates(&ci);
        assert!(!cs
            .iter()
            .any(|a| matches!(a.kind, ActionKind::TriageWait { .. })));
    }

    #[test]
    fn ci_failure_takes_precedence_over_triage_or_wait() {
        let mut ci = empty_ci();
        ci.required.failed = vec![failed("Lint")];
        ci.missing_names = vec![cn("Mergeability Check")];
        ci.advisory.failed = vec![failed("Style")];
        let cs = candidates(&ci);
        // First action is a fix_ci (failures first); triage may or
        // may not also fire, but fix_ci leads.
        assert!(matches!(cs[0].kind, ActionKind::FixCi { .. }));
    }
}
