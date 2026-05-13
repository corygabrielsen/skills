//! Local `codex review` observations — filesystem scan of the
//! per-level batch directories. No subprocess spawn, no network.
//!
//! The unified binary owns the codex review axis as an additional
//! orient surface on top of `/ooda-pr`. Per-PR codex state lives
//! under the recorder's `pr_root/codex/` subtree, with one batch
//! directory per `(reasoning_level, head_sha)` pair. Stale batches
//! (mismatched head_sha.txt) are reported as `NotStarted` so the
//! runner re-spawns at the current PR head.

pub mod batch;
pub mod verdict;

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ids::ReasoningLevel;

pub use batch::{BatchState, VerdictRecord};
pub use verdict::VerdictClass;

/// Per-level observation. The codex review axis observes one level
/// per iteration — the current level — and reports its batch state.
/// The ladder-climb logic lives in orient.
#[derive(Debug, Clone, Serialize)]
pub struct CodexLevelObservation {
    pub level: ReasoningLevel,
    pub batch_state: BatchState,
    pub batch_dir: PathBuf,
}

/// Observation of the entire ladder slice `[floor, ceiling]`, one
/// entry per level. Orient walks this to find the current level and
/// emits the corresponding action.
#[derive(Debug, Clone, Serialize)]
pub struct CodexObservations {
    pub levels: Vec<CodexLevelObservation>,
    pub expected: u32,
    pub head_sha: String,
    pub floor: ReasoningLevel,
    pub ceiling: ReasoningLevel,
}

/// Per-level batch directory: `<pr_codex_root>/levels/<level>/<head_sha_short>`.
/// `head_sha_short` is the first 12 chars of the head SHA so the
/// path stays bounded and prior heads' batches survive on disk as
/// cache (orient ignores them once head changes).
pub fn batch_dir(pr_codex_root: &Path, level: ReasoningLevel, head_sha: &str) -> PathBuf {
    let short = head_sha.get(..12).unwrap_or(head_sha);
    pr_codex_root
        .join("levels")
        .join(level.as_str())
        .join(short)
}

/// Scan the ladder slice `[floor, ceiling]` against the current
/// head SHA. Each level reports its own batch state; orient picks
/// the highest already-clean prefix and emits an action for the
/// next.
pub fn fetch_all(
    pr_codex_root: &Path,
    floor: ReasoningLevel,
    ceiling: ReasoningLevel,
    expected: u32,
    head_sha: &str,
) -> io::Result<CodexObservations> {
    let mut levels = Vec::new();
    for level in ladder_slice(floor, ceiling) {
        let dir = batch_dir(pr_codex_root, level, head_sha);
        let batch_state = batch::scan_batch(&dir, level, expected, head_sha)?;
        levels.push(CodexLevelObservation {
            level,
            batch_state,
            batch_dir: dir,
        });
    }
    Ok(CodexObservations {
        levels,
        expected,
        head_sha: head_sha.to_string(),
        floor,
        ceiling,
    })
}

/// The inclusive level slice `[floor, ceiling]`.
pub fn ladder_slice(floor: ReasoningLevel, ceiling: ReasoningLevel) -> Vec<ReasoningLevel> {
    let mut out = Vec::new();
    let mut cur = floor;
    loop {
        out.push(cur);
        if cur == ceiling {
            break;
        }
        match cur.higher() {
            Some(next) => cur = next,
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
            ladder_slice(ReasoningLevel::Low, ReasoningLevel::High),
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ]
        );
        assert_eq!(
            ladder_slice(ReasoningLevel::Medium, ReasoningLevel::Medium),
            vec![ReasoningLevel::Medium]
        );
        assert_eq!(
            ladder_slice(ReasoningLevel::Low, ReasoningLevel::Xhigh).len(),
            4
        );
    }

    #[test]
    fn batch_dir_uses_short_head_sha() {
        let p = batch_dir(
            Path::new("/tmp/codex"),
            ReasoningLevel::Low,
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
            ReasoningLevel::Low,
            ReasoningLevel::High,
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
