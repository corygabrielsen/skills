//! Codex review orient axis.
//!
//! Walks the observed ladder slice and reports the per-PR axis
//! state: which level the loop is "currently at" (the first level
//! from the floor whose batch isn't `Complete { all-clean }`),
//! plus the verdict records for that level (used when emitting an
//! `AddressCodexReviewBatch` handoff).
//!
//! When every level in `[floor, ceiling]` is clean, the axis is
//! `LadderSatisfied`: no candidates emitted, the PR is free to
//! request approval / merge on its other axes.

use std::path::PathBuf;

use serde::Serialize;

use crate::ids::CodexReasoningLevel;
use crate::observe::codex::VerdictClass;
use crate::observe::codex::{BatchState, CodexLevelObservation, CodexObservations, VerdictRecord};

/// Where on the ladder the axis is right now. Mirrors the codex
/// review state machine: spawn → poll → address → climb.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodexReviewStatus {
    /// `current_level` has no in-flight batch for the current head
    /// SHA — runner should spawn `RunCodexReviewBatch`.
    Spawn { level: CodexReasoningLevel },
    /// `current_level`'s batch is streaming.
    Await {
        level: CodexReasoningLevel,
        total: u32,
        completed: u32,
    },
    /// `current_level` completed with issues — address-batch handoff.
    Address {
        level: CodexReasoningLevel,
        verdicts: Vec<VerdictRecord>,
    },
    /// Every level in [floor, ceiling] is `Complete { all-clean }`
    /// — the axis is satisfied; no candidates this iteration.
    LadderSatisfied,
}

/// Per-PR codex review report. `status` drives decide; the other
/// fields are diagnostic context for renderers and recorders.
#[derive(Debug, Clone, Serialize)]
pub struct CodexReviewReport {
    pub status: CodexReviewStatus,
    pub floor: CodexReasoningLevel,
    pub ceiling: CodexReasoningLevel,
    pub head_sha: String,
    pub expected: u32,
    /// The batch directory for the `current_level` slot, even when
    /// `status = LadderSatisfied` (in which case it points at the
    /// ceiling level — the most-recent clean batch).
    pub current_batch_dir: PathBuf,
    pub current_level: CodexReasoningLevel,
}

/// Project a `CodexObservations` into the orient axis.
pub fn orient_codex_review(obs: &CodexObservations) -> CodexReviewReport {
    // Find the first level from the floor whose batch isn't already
    // complete-and-clean for the current head SHA.
    let mut current_level_observation: Option<&CodexLevelObservation> = None;
    for lvl_obs in &obs.levels {
        match &lvl_obs.batch_state {
            BatchState::Complete { verdicts } if all_clean(verdicts) => continue,
            _ => {
                current_level_observation = Some(lvl_obs);
                break;
            }
        }
    }

    match current_level_observation {
        None => {
            // Every level is `Complete { all-clean }`. Anchor the
            // diagnostic fields at the ceiling. `obs.levels` is
            // `NonEmpty<CodexLevelObservation>`, so `last()` is
            // total — no runtime check on cardinality.
            let last = obs.levels.last();
            CodexReviewReport {
                status: CodexReviewStatus::LadderSatisfied,
                floor: obs.floor,
                ceiling: obs.ceiling,
                head_sha: obs.head_sha.clone(),
                expected: obs.expected,
                current_batch_dir: last.batch_dir.clone(),
                current_level: last.level,
            }
        }
        Some(lvl_obs) => {
            let status = match &lvl_obs.batch_state {
                BatchState::NotStarted => CodexReviewStatus::Spawn {
                    level: lvl_obs.level,
                },
                BatchState::Running { total, completed } => CodexReviewStatus::Await {
                    level: lvl_obs.level,
                    total: *total,
                    completed: *completed,
                },
                BatchState::Complete { verdicts } => CodexReviewStatus::Address {
                    level: lvl_obs.level,
                    verdicts: verdicts.clone(),
                },
            };
            CodexReviewReport {
                status,
                floor: obs.floor,
                ceiling: obs.ceiling,
                head_sha: obs.head_sha.clone(),
                expected: obs.expected,
                current_batch_dir: lvl_obs.batch_dir.clone(),
                current_level: lvl_obs.level,
            }
        }
    }
}

fn all_clean(verdicts: &[VerdictRecord]) -> bool {
    verdicts
        .iter()
        .all(|v| matches!(v.class, VerdictClass::Clean))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::codex::batch::VerdictRecord;

    fn lvl_obs(level: CodexReasoningLevel, bs: BatchState) -> CodexLevelObservation {
        CodexLevelObservation {
            level,
            batch_state: bs,
            batch_dir: PathBuf::from(format!("/tmp/{}-dir", level.as_str())),
        }
    }

    fn clean(slot: u32) -> VerdictRecord {
        VerdictRecord {
            slot,
            body: "ok".into(),
            class: VerdictClass::Clean,
        }
    }

    fn has_issues(slot: u32) -> VerdictRecord {
        VerdictRecord {
            slot,
            body: "Review comment: ...".into(),
            class: VerdictClass::HasIssues,
        }
    }

    fn obs(levels: Vec<CodexLevelObservation>) -> CodexObservations {
        CodexObservations {
            levels: ooda_core::NonEmpty::try_from_vec(levels)
                .expect("test setup must construct a non-empty levels list"),
            expected: 3,
            head_sha: "headsha".into(),
            floor: CodexReasoningLevel::Low,
            ceiling: CodexReasoningLevel::Xhigh,
        }
    }

    #[test]
    fn empty_ladder_spawns_at_floor() {
        let o = obs(vec![lvl_obs(
            CodexReasoningLevel::Low,
            BatchState::NotStarted,
        )]);
        let r = orient_codex_review(&o);
        match r.status {
            CodexReviewStatus::Spawn { level } => assert_eq!(level, CodexReasoningLevel::Low),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn running_at_floor_emits_await() {
        let o = obs(vec![lvl_obs(
            CodexReasoningLevel::Low,
            BatchState::Running {
                total: 3,
                completed: 1,
            },
        )]);
        let r = orient_codex_review(&o);
        match r.status {
            CodexReviewStatus::Await {
                level,
                total,
                completed,
            } => {
                assert_eq!(level, CodexReasoningLevel::Low);
                assert_eq!(total, 3);
                assert_eq!(completed, 1);
            }
            other => panic!("expected Await, got {other:?}"),
        }
    }

    #[test]
    fn complete_with_issues_emits_address() {
        let o = obs(vec![lvl_obs(
            CodexReasoningLevel::Low,
            BatchState::Complete {
                verdicts: vec![clean(1), has_issues(2), clean(3)],
            },
        )]);
        let r = orient_codex_review(&o);
        match r.status {
            CodexReviewStatus::Address { level, verdicts } => {
                assert_eq!(level, CodexReasoningLevel::Low);
                assert_eq!(verdicts.len(), 3);
            }
            other => panic!("expected Address, got {other:?}"),
        }
    }

    #[test]
    fn all_clean_at_floor_advances_to_next_level() {
        let o = obs(vec![
            lvl_obs(
                CodexReasoningLevel::Low,
                BatchState::Complete {
                    verdicts: vec![clean(1), clean(2), clean(3)],
                },
            ),
            lvl_obs(CodexReasoningLevel::Medium, BatchState::NotStarted),
        ]);
        let r = orient_codex_review(&o);
        match r.status {
            CodexReviewStatus::Spawn { level } => assert_eq!(level, CodexReasoningLevel::Medium),
            other => panic!("expected Spawn(Medium), got {other:?}"),
        }
    }

    #[test]
    fn all_levels_clean_emits_ladder_satisfied() {
        let o = obs(vec![
            lvl_obs(
                CodexReasoningLevel::Low,
                BatchState::Complete {
                    verdicts: vec![clean(1), clean(2), clean(3)],
                },
            ),
            lvl_obs(
                CodexReasoningLevel::Medium,
                BatchState::Complete {
                    verdicts: vec![clean(1), clean(2), clean(3)],
                },
            ),
        ]);
        let r = orient_codex_review(&o);
        assert!(matches!(r.status, CodexReviewStatus::LadderSatisfied));
        assert_eq!(r.current_level, CodexReasoningLevel::Medium);
    }
}
