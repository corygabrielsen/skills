//! Observe surface for the local-reviewer axis.
//!
//! # Model
//!
//! A *reviewer axis* climbs a totally-ordered *rigor ladder*
//! `[floor, ceiling]`. At each ladder level the axis owns a *batch*
//! — a fan-out of `expected` independent reviewer runs over a
//! single *target identity* (here: the PR head SHA). The axis
//! advances one level at a time; a level is *cleared* when its
//! batch terminates with no flagged issues.
//!
//! # Invariants
//!
//! - **Pure observation**: this module performs filesystem reads
//!   only — no subprocess spawn, no network, no mutation. The
//!   reported state is a function of the on-disk batch artifacts
//!   and the supplied target identity.
//! - **Identity gating**: a batch is bound to the target identity
//!   it was spawned against. A batch whose recorded identity does
//!   not match the supplied identity is reported as not-started,
//!   forcing re-spawn. This is the entire mechanism by which a
//!   change to the target invalidates prior batches.
//! - **Slice non-emptiness**: `floor ≤ ceiling` (a CLI-level
//!   invariant) implies the ladder slice is non-empty; carried
//!   structurally as `NonEmpty<...>` so downstream code never
//!   `.expect`s on cardinality.

pub(crate) mod batch;
pub(crate) mod verdict;

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ids::CodexReasoningLevel;

pub(crate) use batch::{BatchState, VerdictRecord};
pub(crate) use verdict::VerdictClass;

/// Batch state at a single ladder level, paired with the directory
/// it was scanned from. The ladder-climb decision is downstream.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexLevelObservation {
    pub level: CodexReasoningLevel,
    pub batch_state: BatchState,
    pub batch_dir: PathBuf,
}

/// Snapshot of every level in the slice `[floor, ceiling]` against
/// a single target identity.
///
/// `levels` is `NonEmpty<...>`: `floor ≤ ceiling` implies the slice
/// is non-empty, and that fact is carried in the type so downstream
/// code is total on `.last()` etc.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexObservations {
    pub levels: ooda_core::NonEmpty<CodexLevelObservation>,
    pub expected: u32,
    pub head_sha: String,
    pub floor: CodexReasoningLevel,
    pub ceiling: CodexReasoningLevel,
}

/// Path of the batch directory keyed by `(level, target_identity)`.
///
/// Layout: `<root>/levels/<level>/<identity_prefix>`. The identity
/// is truncated to a bounded prefix so the path stays length-stable;
/// the gate against stale batches lives in the on-disk `head_sha.txt`
/// stamp, not in the path. Prior-identity directories accumulate as
/// a local cache and are ignored by the gate.
pub(crate) fn batch_dir(
    pr_codex_root: &Path,
    level: CodexReasoningLevel,
    head_sha: &str,
) -> PathBuf {
    let short = head_sha.get(..12).unwrap_or(head_sha);
    pr_codex_root
        .join("levels")
        .join(level.as_str())
        .join(short)
}

/// Snapshot every level in `[floor, ceiling]` against `head_sha`.
/// Per-level scans are independent; failures short-circuit.
pub(crate) fn fetch_all(
    pr_codex_root: &Path,
    floor: CodexReasoningLevel,
    ceiling: CodexReasoningLevel,
    expected: u32,
    head_sha: &str,
) -> io::Result<CodexObservations> {
    // `try_map` preserves `NonEmpty` through fallible per-level
    // mapping — non-emptiness is structural, not asserted.
    let levels = ladder_slice(floor, ceiling).try_map(|level| {
        let dir = batch_dir(pr_codex_root, level, head_sha);
        let batch_state = batch::scan_batch(&dir, level, expected, head_sha)?;
        io::Result::Ok(CodexLevelObservation {
            level,
            batch_state,
            batch_dir: dir,
        })
    })?;
    Ok(CodexObservations {
        levels,
        expected,
        head_sha: head_sha.to_string(),
        floor,
        ceiling,
    })
}

/// The inclusive ladder slice `[floor, ceiling]`. Non-empty by
/// construction: `floor` is seeded before any termination check, so
/// `floor == ceiling` yields a singleton.
pub(crate) fn ladder_slice(
    floor: CodexReasoningLevel,
    ceiling: CodexReasoningLevel,
) -> ooda_core::NonEmpty<CodexReasoningLevel> {
    let mut out = ooda_core::NonEmpty::singleton(floor);
    let mut cur = floor;
    while cur != ceiling {
        match cur.higher() {
            Some(next) => {
                out.push(next);
                cur = next;
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_slice_inclusive() {
        assert_eq!(
            ladder_slice(CodexReasoningLevel::Low, CodexReasoningLevel::High).as_slice(),
            &[
                CodexReasoningLevel::Low,
                CodexReasoningLevel::Medium,
                CodexReasoningLevel::High,
            ]
        );
        assert_eq!(
            ladder_slice(CodexReasoningLevel::Medium, CodexReasoningLevel::Medium).as_slice(),
            &[CodexReasoningLevel::Medium]
        );
        assert_eq!(
            ladder_slice(CodexReasoningLevel::Low, CodexReasoningLevel::Xhigh).len(),
            4
        );
    }

    #[test]
    fn batch_dir_uses_short_head_sha() {
        let p = batch_dir(
            Path::new("/tmp/codex"),
            CodexReasoningLevel::Low,
            "abcdef0123456789",
        );
        assert_eq!(p, PathBuf::from("/tmp/codex/levels/low/abcdef012345"));
    }

    #[test]
    fn fetch_all_returns_not_started_for_missing_dirs() {
        let root = std::env::temp_dir().join(format!(
            "ooda-pr-codex-review-fetch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let obs = fetch_all(
            &root,
            CodexReasoningLevel::Low,
            CodexReasoningLevel::High,
            3,
            "headsha12345abc",
        )
        .unwrap();
        assert_eq!(obs.levels.len(), 3);
        for lvl in obs.levels {
            assert_eq!(lvl.batch_state, BatchState::NotStarted);
        }
    }
}
