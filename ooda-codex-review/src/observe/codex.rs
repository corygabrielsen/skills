//! Codex-domain observations.
//!
//! Submodules split by concern:
//!   - [`verdict`] — pure text extraction + classification of the
//!     `codex` verdict block within a single log file.
//!   - [`batch`] — filesystem scan of the per-level batch directory
//!     to produce a [`BatchState`].
//!
//! [`fetch_all`] is the boundary entry point; the runner closure
//! calls it once per OODA iteration to refresh observations.

pub mod batch;
pub mod verdict;

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::decide::action::ReasoningLevel;
use crate::ids::{RepoId, ReviewTarget};

use batch::{BatchState, scan_batch};

pub use verdict::VerdictClass;

/// Per-iteration observation bundle. The runner closure constructs
/// this each iteration; orient consumes it. Phase 5 wires
/// `current_level` and `batch_state`; Phase 6 will add
/// `level_history` and `last_test_run` once the recorder is the
/// source of truth for cross-iteration state.
#[derive(Debug, Clone, Serialize)]
pub struct CodexObservations {
    pub repo_id: RepoId,
    pub target: ReviewTarget,
    pub current_level: ReasoningLevel,
    pub batch_state: BatchState,
    /// The directory the batch was scanned from. Recorded so the
    /// orient/decide layers can reference log paths in handoff
    /// prompts without re-deriving the layout.
    pub batch_dir: PathBuf,
    /// Configured `n` for this batch — how many `codex review`
    /// processes were launched. Forwarded from the caller; observe
    /// does not derive it. Decide uses it as the floor for
    /// `pending = expected - completed`.
    pub expected: u32,
}

/// Read filesystem state for one OODA iteration.
///
/// Inputs:
///   - `repo_id`, `target`: identity, passed through into the
///     observation bundle for downstream rendering / recorder
///     keying. Observe never derives them.
///   - `batch_dir`: where the per-level log files live.
///   - `level`: which `{level}-*.log` files to scan.
///   - `expected`: the configured `n` for this batch.
///
/// All filesystem reads are scoped to `batch_dir`; no subprocess
/// spawn, no network. Spawn lives in `act` (Phase 5/7 tie-in).
pub fn fetch_all(
    repo_id: RepoId,
    target: ReviewTarget,
    batch_dir: &Path,
    level: ReasoningLevel,
    expected: u32,
) -> io::Result<CodexObservations> {
    let batch_state = scan_batch(batch_dir, level, expected)?;
    Ok(CodexObservations {
        repo_id,
        target,
        current_level: level,
        batch_state,
        batch_dir: batch_dir.to_path_buf(),
        expected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_batch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ooda-codex-review-fetch-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn dummy_repo_id() -> RepoId {
        RepoId::parse("ooda-codex-review-deadbeef").unwrap()
    }

    #[test]
    fn fetch_all_passes_through_identity_and_scans_batch() {
        let dir = temp_batch_dir("passthrough");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\ncodex\nLooks good\n").unwrap();

        let obs = fetch_all(
            dummy_repo_id(),
            ReviewTarget::Uncommitted,
            &dir,
            ReasoningLevel::Low,
            2,
        )
        .unwrap();

        assert_eq!(obs.repo_id.as_str(), "ooda-codex-review-deadbeef");
        assert_eq!(obs.target, ReviewTarget::Uncommitted);
        assert_eq!(obs.current_level, ReasoningLevel::Low);
        assert_eq!(obs.batch_dir, dir);
        match obs.batch_state {
            BatchState::Complete { verdicts } => assert_eq!(verdicts.len(), 2),
            other => panic!("expected Complete, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fetch_all_returns_not_started_for_empty_batch_dir() {
        let dir = temp_batch_dir("empty");
        let obs = fetch_all(
            dummy_repo_id(),
            ReviewTarget::Uncommitted,
            &dir,
            ReasoningLevel::High,
            5,
        )
        .unwrap();
        assert_eq!(obs.batch_state, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }
}
