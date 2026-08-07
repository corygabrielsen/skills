//! Orient projection for the reviewer-ladder axis.
//!
//! # Model
//!
//! Given per-level batch observations across `[floor, ceiling]`,
//! the axis is in exactly one of four phases at any instant:
//! *spawn* a new batch at the current level, *await* the running
//! batch, *address* its issues, or *satisfied* (every level cleared).
//!
//! # Invariants
//!
//! - **Current level = first uncleared**: the axis projects to the
//!   lowest level whose batch is not `Complete { all-clean }`. The
//!   ladder is climbed monotonically; a cleared level never reverts.
//! - **Ladder satisfaction is total**: every level cleared maps to
//!   `LadderSatisfied`. The axis emits no work; downstream gates
//!   (merge, approval) are free to advance.
//! - **No-candidate is the unblock signal**: `LadderSatisfied`
//!   producing no candidate is what structurally gates merge on the
//!   axis converging.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::ids::CodexReasoningLevel;
use crate::observe::codex::VerdictClass;
use crate::observe::codex::batch::ALIVE_THRESHOLD;
use crate::observe::codex::{BatchState, CodexLevelObservation, CodexObservations, VerdictRecord};

/// The phase the axis is in at the current level.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CodexReviewStatus {
    /// No batch exists for the current target identity at `level`.
    Spawn { level: CodexReasoningLevel },
    /// Batch at `level` is streaming; `completed ≤ total`.
    Await {
        level: CodexReasoningLevel,
        total: u32,
        completed: u32,
    },
    /// Batch at `level` completed with at least one non-clean verdict.
    Address {
        level: CodexReasoningLevel,
        verdicts: Vec<VerdictRecord>,
    },
    /// Stale state observed at `level` (more completed slots than
    /// expected — stray log file or mismatched caller `n`). No safe
    /// auto-recovery; surface for human resolution.
    Inconsistent {
        level: CodexReasoningLevel,
        reason: String,
    },
    /// Every level in `[floor, ceiling]` is `Complete { all-clean }`.
    LadderSatisfied,
}

/// Axis report. `status` drives the decide layer; the remaining
/// fields are diagnostic context anchored to a well-defined level
/// even when the axis is satisfied.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexReviewReport {
    pub status: CodexReviewStatus,
    pub floor: CodexReasoningLevel,
    pub ceiling: CodexReasoningLevel,
    pub head_sha: String,
    pub expected: u32,
    /// Batch directory for `current_level`. When the axis is
    /// satisfied, anchors at the ceiling level's most recent clean
    /// batch.
    pub current_batch_dir: PathBuf,
    pub current_level: CodexReasoningLevel,
}

/// Project per-level observations into the axis report.
pub(crate) fn orient_codex_review(obs: &CodexObservations) -> CodexReviewReport {
    // Current level = first uncleared (module invariant).
    let mut current_level_observation: Option<&CodexLevelObservation> = None;
    for lvl_obs in &obs.levels {
        let is_clean_complete = matches!(
            &lvl_obs.batch_state,
            BatchState::Complete { verdicts } if all_clean(verdicts)
        );
        if !is_clean_complete {
            current_level_observation = Some(lvl_obs);
            break;
        }
    }

    match current_level_observation {
        None => {
            // Ladder satisfied — anchor diagnostics at ceiling.
            // `last()` is total: `obs.levels` is `NonEmpty<_>`.
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
            // Alive/idle discriminator — mirrors ooda-codex-review's
            // decide layer: a Running batch where EVERY pending slot
            // has been idle past [`ALIVE_THRESHOLD`] is promoted to a
            // synthetic Complete with the pending slots Abandoned,
            // then falls through the normal status fork. Any slot
            // still streaming keeps the batch in Await — the "slow
            // but progressing" tail is never abandoned mid-flight.
            // Abandoned counts as non-clean downstream, so a partial
            // sample never silently claims fixed point.
            let projected = discriminate_running(&lvl_obs.batch_state, SystemTime::now());
            let batch_state = projected.as_ref().unwrap_or(&lvl_obs.batch_state);
            let status = match batch_state {
                BatchState::NotStarted => CodexReviewStatus::Spawn {
                    level: lvl_obs.level,
                },
                BatchState::Running {
                    pending_slots,
                    completed_verdicts,
                } => CodexReviewStatus::Await {
                    level: lvl_obs.level,
                    // Widths are bounded by the batch fan-out; u32 fits.
                    total: u32::try_from(pending_slots.len() + completed_verdicts.len())
                        .expect("batch slot count fits in u32"),
                    completed: u32::try_from(completed_verdicts.len())
                        .expect("batch verdict count fits in u32"),
                },
                BatchState::Complete { verdicts } => CodexReviewStatus::Address {
                    level: lvl_obs.level,
                    verdicts: verdicts.clone(),
                },
                BatchState::InconsistentState { reason, .. } => CodexReviewStatus::Inconsistent {
                    level: lvl_obs.level,
                    reason: reason.clone(),
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

/// Promote a `Running` batch whose pending slots are ALL idle past
/// [`ALIVE_THRESHOLD`] to a synthetic `Complete` with those slots
/// `Abandoned`. `None` when the state is not `Running` or any slot
/// is still alive.
fn discriminate_running(bs: &BatchState, now: SystemTime) -> Option<BatchState> {
    let BatchState::Running { pending_slots, .. } = bs else {
        return None;
    };
    let any_alive = pending_slots
        .iter()
        .any(|s| is_alive(s.log_mtime, now, ALIVE_THRESHOLD));
    if any_alive {
        return None;
    }
    bs.project_abandoning_pending("all pending slots idle past alive threshold")
}

/// `true` iff `mtime` is within `threshold` of `now` — the slot has
/// produced log output recently enough that the codex subprocess is
/// presumed to still be making forward progress.
fn is_alive(mtime: SystemTime, now: SystemTime, threshold: Duration) -> bool {
    now.duration_since(mtime).is_ok_and(|d| d <= threshold)
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
            BatchState::running_alive(1, 3),
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
    fn running_with_all_pending_idle_projects_to_address() {
        // Alive/idle discriminator: every pending slot idle past
        // ALIVE_THRESHOLD (epoch mtimes) → the batch is promoted to
        // a synthetic Complete whose pending slots are Abandoned,
        // and the axis surfaces Address instead of waiting forever.
        use crate::observe::codex::batch::PendingSlot;
        let bs = BatchState::Running {
            pending_slots: vec![
                PendingSlot {
                    slot: 2,
                    log_mtime: SystemTime::UNIX_EPOCH,
                    log_bytes: 10,
                },
                PendingSlot {
                    slot: 3,
                    log_mtime: SystemTime::UNIX_EPOCH,
                    log_bytes: 0,
                },
            ],
            completed_verdicts: vec![clean(1)],
        };
        let o = obs(vec![lvl_obs(CodexReasoningLevel::Low, bs)]);
        let r = orient_codex_review(&o);
        match r.status {
            CodexReviewStatus::Address { level, verdicts } => {
                assert_eq!(level, CodexReasoningLevel::Low);
                assert_eq!(verdicts.len(), 3);
                assert!(matches!(verdicts[0].class, VerdictClass::Clean));
                assert!(matches!(verdicts[1].class, VerdictClass::Abandoned));
                assert!(matches!(verdicts[2].class, VerdictClass::Abandoned));
            }
            other => panic!("expected Address, got {other:?}"),
        }
    }

    #[test]
    fn running_with_one_alive_slot_stays_await() {
        // One slot still streaming (fresh mtime) — the discriminator
        // must not abandon: Await, not Address.
        use crate::observe::codex::batch::PendingSlot;
        let bs = BatchState::Running {
            pending_slots: vec![
                PendingSlot {
                    slot: 2,
                    log_mtime: SystemTime::UNIX_EPOCH,
                    log_bytes: 10,
                },
                PendingSlot {
                    slot: 3,
                    log_mtime: SystemTime::now(),
                    log_bytes: 512,
                },
            ],
            completed_verdicts: vec![clean(1)],
        };
        let o = obs(vec![lvl_obs(CodexReasoningLevel::Low, bs)]);
        let r = orient_codex_review(&o);
        match r.status {
            CodexReviewStatus::Await {
                total, completed, ..
            } => {
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
