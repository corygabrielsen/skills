//! Decide-side candidate generator for the codex review axis.
//!
//! Maps `CodexReviewReport.status` to one `Action`:
//!
//! ```text
//! Spawn{level}                 → RunCodexReviewBatch{level, n}      Full,  Critical
//! Await{level, ...}            → AwaitCodexReviewBatch{level}       Wait,  BlockingWait
//! Address{level, verdicts}     → AddressCodexReviewBatch{level, n}  Agent, BlockingFix
//! LadderSatisfied              → no candidate (axis empty)
//! ```
//!
//! `LadderSatisfied` returning no candidate is what lets the PR
//! axes (`RequestApproval`, eventual merge) progress: the codex
//! review axis emits a `BlockingFix`/`BlockingWait` candidate
//! whenever it has work, structurally gating merge on codex's
//! fixed point.

use crate::ids::{BlockerKey, ReasoningLevel};
use crate::observe::codex::VerdictClass;
use crate::orient::codex_review::{CodexReviewReport, CodexReviewStatus};

use super::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};

const AWAIT_INTERVAL_SECS: u64 = 30;

pub(crate) fn candidates(report: &CodexReviewReport) -> Vec<Action> {
    match &report.status {
        CodexReviewStatus::LadderSatisfied => Vec::new(),
        CodexReviewStatus::Spawn { level } => vec![mk_run(*level, report.expected)],
        CodexReviewStatus::Await {
            level,
            total,
            completed,
        } => {
            let pending = total.saturating_sub(*completed);
            vec![mk_await(*level, pending)]
        }
        CodexReviewStatus::Address { level, verdicts } => {
            let issues: Vec<&crate::observe::codex::VerdictRecord> = verdicts
                .iter()
                .filter(|v| matches!(v.class, VerdictClass::HasIssues))
                .collect();
            let count = issues.len() as u32;
            vec![mk_address(*level, count, &issues)]
        }
    }
}

fn mk_run(level: ReasoningLevel, n: u32) -> Action {
    Action {
        kind: ActionKind::RunCodexReviewBatch { level, n },
        effect: ActionEffect::Full {
            log: format!(
                "Spawn {n} codex review subprocesses at reasoning level {}.",
                level.as_str()
            ),
        },
        target_effect: TargetEffect::Advances,
        urgency: Urgency::Critical,
        blocker: BlockerKey::tag(format!("codex_review_runbatch:{}", level.as_str())),
    }
}

fn mk_await(level: ReasoningLevel, pending: u32) -> Action {
    Action {
        kind: ActionKind::AwaitCodexReviewBatch { level, pending },
        effect: ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(AWAIT_INTERVAL_SECS),
            log: format!(
                "Polling: {pending} codex review(s) still streaming at level {}.",
                level.as_str()
            ),
        },
        target_effect: TargetEffect::Neutral,
        urgency: Urgency::BlockingWait,
        blocker: BlockerKey::tag(format!("codex_review_await:{}", level.as_str())),
    }
}

fn mk_address(
    level: ReasoningLevel,
    count: u32,
    issues: &[&crate::observe::codex::VerdictRecord],
) -> Action {
    use ooda_core::{HandoffPrompt, NonEmpty, SingleLineString, Witness};

    let headline = format!(
        "Verify and address {count} codex review(s) with issues at level {}. \
         For each issue: real bug → fix; false positive → clarify code; \
         design tradeoff → document rationale. Then push the fix; the PR \
         loop will observe the new head and re-run codex review at this level.",
        level.as_str()
    );
    let mut prompt = HandoffPrompt::new(headline);
    let witnesses: Vec<Witness> = issues
        .iter()
        .map(|v| Witness {
            label: SingleLineString::new(format!("— slot {} —", v.slot)),
            body: v.body.trim_end().to_string(),
        })
        .collect();
    // `mk_address` is only invoked when at least one verdict has
    // issues (decide-side filter above); empty witnesses would
    // mean an empty Address candidate, which the caller skips.
    if let Some(witnesses) = NonEmpty::try_from_vec(witnesses) {
        prompt.push_witnesses(witnesses);
    }
    Action {
        kind: ActionKind::AddressCodexReviewBatch { level, count },
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::BlockingFix,
        blocker: BlockerKey::tag(format!("codex_review_address:{}", level.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::codex::VerdictRecord;
    use std::path::PathBuf;

    fn report(status: CodexReviewStatus) -> CodexReviewReport {
        CodexReviewReport {
            status,
            floor: ReasoningLevel::Low,
            ceiling: ReasoningLevel::Xhigh,
            head_sha: "headsha".into(),
            expected: 3,
            current_batch_dir: PathBuf::from("/tmp/x"),
            current_level: ReasoningLevel::Low,
        }
    }

    #[test]
    fn spawn_emits_run_full_critical() {
        let r = report(CodexReviewStatus::Spawn {
            level: ReasoningLevel::Low,
        });
        let cs = candidates(&r);
        assert_eq!(cs.len(), 1);
        assert!(matches!(
            cs[0].kind,
            ActionKind::RunCodexReviewBatch {
                level: ReasoningLevel::Low,
                n: 3
            }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Full { .. }));
        assert_eq!(cs[0].urgency, Urgency::Critical);
    }

    #[test]
    fn await_emits_wait() {
        let r = report(CodexReviewStatus::Await {
            level: ReasoningLevel::Medium,
            total: 3,
            completed: 1,
        });
        let cs = candidates(&r);
        assert_eq!(cs.len(), 1);
        assert!(matches!(
            cs[0].kind,
            ActionKind::AwaitCodexReviewBatch {
                level: ReasoningLevel::Medium,
                pending: 2
            }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Wait { .. }));
        assert_eq!(cs[0].urgency, Urgency::BlockingWait);
    }

    #[test]
    fn address_emits_agent_with_only_has_issues_counted() {
        let r = report(CodexReviewStatus::Address {
            level: ReasoningLevel::High,
            verdicts: vec![
                VerdictRecord {
                    slot: 1,
                    body: "ok".into(),
                    class: VerdictClass::Clean,
                },
                VerdictRecord {
                    slot: 2,
                    body: "Review comment: src/foo.rs:1".into(),
                    class: VerdictClass::HasIssues,
                },
                VerdictRecord {
                    slot: 3,
                    body: "Review comment: src/bar.rs:2".into(),
                    class: VerdictClass::HasIssues,
                },
            ],
        });
        let cs = candidates(&r);
        assert_eq!(cs.len(), 1);
        match &cs[0].kind {
            ActionKind::AddressCodexReviewBatch { level, count } => {
                assert_eq!(*level, ReasoningLevel::High);
                assert_eq!(*count, 2);
            }
            other => panic!("expected AddressCodexReviewBatch, got {other:?}"),
        }
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::BlockingFix);
        // Description bundles verdict bodies.
        assert!(cs[0].rendered_payload().contains("slot 2"));
        assert!(cs[0].rendered_payload().contains("slot 3"));
    }

    #[test]
    fn ladder_satisfied_emits_no_candidates() {
        let r = report(CodexReviewStatus::LadderSatisfied);
        assert!(candidates(&r).is_empty());
    }

    // ─── property test for the class invariant ──────────────────────
    //
    // Class invariant: every `CodexReviewStatus` variant maps to a
    // unique candidate-set shape. The exhaustive match in
    // `expected_codex_review_axis_behavior` is the contract. Adding
    // a new `CodexReviewStatus` variant fails to compile here until
    // the new arm is added.

    #[derive(Debug, PartialEq, Eq)]
    enum CodexReviewAxisBehavior {
        /// Empty candidate set — ladder is satisfied, nothing to do.
        NoCandidate,
        EmitRunBatch,
        EmitAwaitBatch,
        EmitAddressBatch,
    }

    fn expected_codex_review_axis_behavior(status: &CodexReviewStatus) -> CodexReviewAxisBehavior {
        match status {
            CodexReviewStatus::LadderSatisfied => CodexReviewAxisBehavior::NoCandidate,
            CodexReviewStatus::Spawn { .. } => CodexReviewAxisBehavior::EmitRunBatch,
            CodexReviewStatus::Await { .. } => CodexReviewAxisBehavior::EmitAwaitBatch,
            CodexReviewStatus::Address { .. } => CodexReviewAxisBehavior::EmitAddressBatch,
        }
    }

    fn all_codex_review_statuses() -> Vec<CodexReviewStatus> {
        vec![
            CodexReviewStatus::LadderSatisfied,
            CodexReviewStatus::Spawn {
                level: ReasoningLevel::Low,
            },
            CodexReviewStatus::Await {
                level: ReasoningLevel::Medium,
                total: 3,
                completed: 1,
            },
            CodexReviewStatus::Address {
                level: ReasoningLevel::High,
                verdicts: vec![VerdictRecord {
                    slot: 1,
                    body: "needs fix".into(),
                    class: VerdictClass::HasIssues,
                }],
            },
        ]
    }

    fn observed_codex_review_axis_behavior(cs: &[Action]) -> CodexReviewAxisBehavior {
        match cs {
            [] => CodexReviewAxisBehavior::NoCandidate,
            [a] => match (&a.kind, &a.effect) {
                (ActionKind::RunCodexReviewBatch { .. }, ActionEffect::Full { .. }) => {
                    CodexReviewAxisBehavior::EmitRunBatch
                }
                (ActionKind::AwaitCodexReviewBatch { .. }, ActionEffect::Wait { .. }) => {
                    CodexReviewAxisBehavior::EmitAwaitBatch
                }
                (ActionKind::AddressCodexReviewBatch { .. }, ActionEffect::Agent { .. }) => {
                    CodexReviewAxisBehavior::EmitAddressBatch
                }
                (kind, effect) => panic!(
                    "codex-review axis emitted unexpected (kind, effect): \
                     {kind:?}, {effect:?}",
                ),
            },
            multi => panic!(
                "codex-review axis emitted unexpected candidate count: {} items",
                multi.len()
            ),
        }
    }

    #[test]
    fn codex_review_axis_property_holds_for_every_status() {
        let statuses = all_codex_review_statuses();
        assert_eq!(
            statuses.len(),
            4,
            "`all_codex_review_statuses` must include one sample per \
             `CodexReviewStatus` variant; adding a new variant requires \
             adding both an arm in `expected_codex_review_axis_behavior` \
             AND a sample here.",
        );
        for status in statuses {
            let r = report(status.clone());
            let cs = candidates(&r);
            let actual = observed_codex_review_axis_behavior(&cs);
            let expected = expected_codex_review_axis_behavior(&status);
            assert_eq!(
                actual, expected,
                "codex-review axis contract violated for status = {status:?}",
            );
        }
    }
}
