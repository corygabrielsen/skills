//! Decide projection for the reviewer-ladder axis.
//!
//! # Mapping
//!
//! Status × candidate-shape is a total function:
//!
//! | Status            | Candidate kind        | Effect | Urgency       |
//! |-------------------|-----------------------|--------|-----------------|
//! | `Spawn`           | `RunCodexReviewBatch` | Full   | `Critical`      |
//! | `Await`           | `AwaitCodexReviewBatch` | Wait | `BlockingWait`  |
//! | `Address`         | `AddressCodexReviewBatch` | Agent | `BlockingFix` |
//! | `LadderSatisfied` | (none)                | —      | —               |
//!
//! # Invariants
//!
//! - **One candidate per non-satisfied status**: cardinality is
//!   exactly one, exercised by the property test below.
//! - **Empty candidate set is the only unblock**: a non-empty axis
//!   gates merge structurally; only `LadderSatisfied` releases the
//!   gate, by returning `Vec::new()`.
//! - **Issue-filter precondition**: `mk_address` is invoked only
//!   when at least one verdict is non-clean, so the witness set is
//!   non-empty by construction.

use crate::ids::{BlockerKey, CodexReasoningLevel};
use crate::observe::codex::VerdictClass;
use crate::orient::codex_review::{CodexReviewReport, CodexReviewStatus};

use super::action::{Action, ActionEffect, ActionKind, MidTier, TargetEffect, Urgency};

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
                .filter(|v| {
                    matches!(
                        v.class,
                        VerdictClass::HasIssues | VerdictClass::Indeterminate
                    )
                })
                .collect();
            // Issue count fits in u32: bounded by batch fan-out.
            let count = u32::try_from(issues.len()).expect("codex issue count fits in u32");
            vec![mk_address(*level, count, &issues)]
        }
        CodexReviewStatus::Inconsistent { level, reason } => {
            vec![mk_inconsistent(*level, reason)]
        }
    }
}

fn mk_run(level: CodexReasoningLevel, n: u32) -> Action {
    Action {
        kind: ActionKind::RunCodexReviewBatch { level, n },
        effect: ActionEffect::Full {
            log: format!(
                "Spawn {n} codex review subprocesses at reasoning level {}.",
                level.as_str()
            ),
        },
        target_effect: TargetEffect::Advances,
        urgency: Urgency::Mid(MidTier::Critical),
        blocker: BlockerKey::typed("codex_review_runbatch", &level),
    }
}

fn mk_await(level: CodexReasoningLevel, pending: u32) -> Action {
    Action {
        kind: ActionKind::AwaitCodexReviewBatch { level, pending },
        effect: ActionEffect::Wait {
            interval: ooda_core::PollingInterval::from_secs(AWAIT_INTERVAL_SECS),
            log: format!(
                "Polling: {pending} codex review(s) still streaming at level {}.",
                level.as_str()
            ),
        },
        // `Blocks`: the axis gates merge until ladder satisfaction.
        // `Neutral` would misclassify the await as non-advancing,
        // letting the merge-policy fallback emit a phantom candidate.
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingWait),
        blocker: BlockerKey::typed("codex_review_await", &level),
    }
}

fn mk_inconsistent(level: CodexReasoningLevel, reason: &str) -> Action {
    use ooda_core::HandoffPrompt;
    Action {
        kind: ActionKind::CodexReviewBatchInconsistent {
            level,
            reason: reason.to_string(),
        },
        effect: ActionEffect::Human {
            prompt: HandoffPrompt::new(format!(
                "Stale codex-review batch state at level {}: {reason}. \
                 The auto-loop has no safe recovery: inspect the batch \
                 directory, clear stray log/exit files (or remove \
                 `head_sha.txt` to force a fresh batch), then re-run.",
                level.as_str(),
            )),
        },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::Pathology),
        blocker: BlockerKey::typed("codex_review_inconsistent", &level),
    }
}

fn mk_address(
    level: CodexReasoningLevel,
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
            body: v.body.trim_end().to_string().into(),
            url: None,
        })
        .collect();
    // Witness set is non-empty by precondition (module invariant);
    // the `try_from_vec` is defensive and falls back to no-op if
    // the precondition is ever violated.
    if let Some(witnesses) = NonEmpty::try_from_vec(witnesses) {
        prompt.push_witnesses(witnesses);
    }
    Action {
        kind: ActionKind::AddressCodexReviewBatch { level, count },
        effect: ActionEffect::Agent { prompt },
        target_effect: TargetEffect::Blocks,
        urgency: Urgency::Mid(MidTier::BlockingFix),
        blocker: BlockerKey::typed("codex_review_address", &level),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::codex::VerdictRecord;
    use ooda_core::MidTier;
    use std::path::PathBuf;

    fn report(status: CodexReviewStatus) -> CodexReviewReport {
        CodexReviewReport {
            status,
            floor: CodexReasoningLevel::Low,
            ceiling: CodexReasoningLevel::Xhigh,
            head_sha: "headsha".into(),
            expected: 3,
            current_batch_dir: PathBuf::from("/tmp/x"),
            current_level: CodexReasoningLevel::Low,
        }
    }

    #[test]
    fn spawn_emits_run_full_critical() {
        let r = report(CodexReviewStatus::Spawn {
            level: CodexReasoningLevel::Low,
        });
        let cs = candidates(&r);
        assert_eq!(cs.len(), 1);
        assert!(matches!(
            cs[0].kind,
            ActionKind::RunCodexReviewBatch {
                level: CodexReasoningLevel::Low,
                n: 3
            }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Full { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Critical));
    }

    #[test]
    fn await_emits_wait() {
        let r = report(CodexReviewStatus::Await {
            level: CodexReasoningLevel::Medium,
            total: 3,
            completed: 1,
        });
        let cs = candidates(&r);
        assert_eq!(cs.len(), 1);
        assert!(matches!(
            cs[0].kind,
            ActionKind::AwaitCodexReviewBatch {
                level: CodexReasoningLevel::Medium,
                pending: 2
            }
        ));
        assert!(matches!(cs[0].effect, ActionEffect::Wait { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingWait));
    }

    #[test]
    fn address_emits_agent_with_only_has_issues_counted() {
        let r = report(CodexReviewStatus::Address {
            level: CodexReasoningLevel::High,
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
                assert_eq!(*level, CodexReasoningLevel::High);
                assert_eq!(*count, 2);
            }
            other => panic!("expected AddressCodexReviewBatch, got {other:?}"),
        }
        assert!(matches!(cs[0].effect, ActionEffect::Agent { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::BlockingFix));
        // Description bundles verdict bodies.
        assert!(cs[0].rendered_payload().contains("slot 2"));
        assert!(cs[0].rendered_payload().contains("slot 3"));
    }

    #[test]
    fn inconsistent_emits_human_handoff() {
        // Site 7 (Site C from F7) regression: a stale-state
        // observation (completed > expected) used to project to
        // `AwaitCodexReviewBatch { pending: 0 }` — a Wait the
        // loop honours forever. The new path emits a Human
        // handoff at the Pathology tier so a resolver can clear
        // the stale state.
        let r = report(CodexReviewStatus::Inconsistent {
            level: CodexReasoningLevel::High,
            reason: "stray log file from prior batch".into(),
        });
        let cs = candidates(&r);
        assert_eq!(cs.len(), 1);
        match &cs[0].kind {
            ActionKind::CodexReviewBatchInconsistent { level, reason } => {
                assert_eq!(*level, CodexReasoningLevel::High);
                assert!(reason.contains("stray log"), "reason: {reason}");
            }
            other => panic!("expected CodexReviewBatchInconsistent, got {other:?}"),
        }
        assert!(matches!(cs[0].effect, ActionEffect::Human { .. }));
        assert_eq!(cs[0].urgency, Urgency::Mid(MidTier::Pathology));
    }

    #[test]
    fn ladder_satisfied_emits_no_candidates() {
        let r = report(CodexReviewStatus::LadderSatisfied);
        assert!(candidates(&r).is_empty());
    }

    // Property test for the status → candidate-shape mapping.
    // Exhaustive match in `expected_codex_review_axis_behavior`
    // enforces the contract at compile time; adding a new
    // `CodexReviewStatus` variant requires extending both the
    // expectation arm and the sample set.

    #[derive(Debug, PartialEq, Eq)]
    enum CodexReviewAxisBehavior {
        /// Empty candidate set — axis is satisfied.
        NoCandidate,
        EmitRunBatch,
        EmitAwaitBatch,
        EmitAddressBatch,
        EmitInconsistent,
    }

    fn expected_codex_review_axis_behavior(status: &CodexReviewStatus) -> CodexReviewAxisBehavior {
        match status {
            CodexReviewStatus::LadderSatisfied => CodexReviewAxisBehavior::NoCandidate,
            CodexReviewStatus::Spawn { .. } => CodexReviewAxisBehavior::EmitRunBatch,
            CodexReviewStatus::Await { .. } => CodexReviewAxisBehavior::EmitAwaitBatch,
            CodexReviewStatus::Address { .. } => CodexReviewAxisBehavior::EmitAddressBatch,
            CodexReviewStatus::Inconsistent { .. } => CodexReviewAxisBehavior::EmitInconsistent,
        }
    }

    fn all_codex_review_statuses() -> Vec<CodexReviewStatus> {
        vec![
            CodexReviewStatus::LadderSatisfied,
            CodexReviewStatus::Spawn {
                level: CodexReasoningLevel::Low,
            },
            CodexReviewStatus::Await {
                level: CodexReasoningLevel::Medium,
                total: 3,
                completed: 1,
            },
            CodexReviewStatus::Address {
                level: CodexReasoningLevel::High,
                verdicts: vec![VerdictRecord {
                    slot: 1,
                    body: "needs fix".into(),
                    class: VerdictClass::HasIssues,
                }],
            },
            CodexReviewStatus::Inconsistent {
                level: CodexReasoningLevel::High,
                reason: "stray log".into(),
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
                (ActionKind::CodexReviewBatchInconsistent { .. }, ActionEffect::Human { .. }) => {
                    CodexReviewAxisBehavior::EmitInconsistent
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
            5,
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
