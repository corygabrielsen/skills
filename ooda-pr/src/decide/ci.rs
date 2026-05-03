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

use super::action::{Action, ActionKind, Automation, TargetEffect, Urgency};
use crate::orient::ci::CiSummary;

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
            blocker: format!("ci_fail: {}", f.name),
        });
    }

    let blocked: Vec<String> = ci
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
        out.push(Action {
            kind: ActionKind::TriageWait {
                blocked_checks: blocked.clone(),
            },
            automation: Automation::Agent,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: desc.join("\n"),
            blocker: format!("ci_triage: {}", blocked.join(", ")),
        });
    } else {
        if !ci.required.pending_names.is_empty() {
            let names = ci.required.pending_names.clone();
            out.push(Action {
                kind: ActionKind::WaitForCi {
                    pending: names.clone(),
                },
                automation: Automation::Wait { seconds: 60 },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                description: format!(
                    "Wait for {} pending check(s)",
                    ci.required.pending(),
                ),
                blocker: format!("ci_pending: {}", names.join(", ")),
            });
        }
        if !ci.missing_names.is_empty() {
            let names = ci.missing_names.clone();
            out.push(Action {
                kind: ActionKind::WaitForCi {
                    pending: names.clone(),
                },
                automation: Automation::Wait { seconds: 60 },
                target_effect: TargetEffect::Blocks,
                urgency: Urgency::BlockingWait,
                description: format!(
                    "{} required check(s) not started: {}",
                    ci.missing(),
                    names.join(", ")
                ),
                blocker: format!("ci_missing: {}", names.join(", ")),
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
            name: name.into(),
            description: String::new(),
            link: String::new(),
        }
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
        ci.required.pending_names = vec!["Build".into(), "Test".into()];
        let cs = candidates(&ci);
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0].kind, ActionKind::WaitForCi { .. }));
        assert!(matches!(cs[0].automation, Automation::Wait { .. }));
    }

    #[test]
    fn missing_required_emits_wait_for_ci_with_separate_blocker() {
        let mut ci = empty_ci();
        ci.missing_names = vec!["Mergeability Check".into()];
        let cs = candidates(&ci);
        assert_eq!(cs.len(), 1);
        assert!(cs[0].blocker.starts_with("ci_missing"));
    }

    #[test]
    fn advisory_failure_with_blocked_required_triggers_triage() {
        let mut ci = empty_ci();
        ci.missing_names = vec!["Mergeability Check".into()];
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
        ci.missing_names = vec!["Mergeability Check".into()];
        ci.advisory.failed = vec![failed("Style")];
        let cs = candidates(&ci);
        // First action is a fix_ci (failures first); triage may or
        // may not also fire, but fix_ci leads.
        assert!(matches!(cs[0].kind, ActionKind::FixCi { .. }));
    }
}
