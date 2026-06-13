//! Codex-domain observations.
//!
//! Two submodules split by concern:
//!
//! - [`verdict`] — pure text extraction + classification of the
//!   verdict block within a single log file.
//! - [`batch`] — filesystem scan of the per-level batch directory
//!   producing a [`BatchState`].
//!
//! [`fetch_all`] is the entry point; the loop calls it once per
//! iteration to refresh observations.

pub(crate) mod batch;
pub(crate) mod verdict;

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::decide::action::CodexReasoningLevel;
use crate::ids::{RepoId, ReviewTarget};

use batch::{BatchState, scan_batch};

pub(crate) use verdict::VerdictClass;

/// Per-iteration observation bundle. Constructed once per
/// iteration; consumed by orient.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexObservations {
    pub repo_id: RepoId,
    pub target: ReviewTarget,
    pub current_level: CodexReasoningLevel,
    pub batch_state: BatchState,
    /// Directory the batch was scanned from. Carried so downstream
    /// layers can cite per-log paths without re-deriving layout.
    pub batch_dir: PathBuf,
    /// Configured batch fan-out. Forwarded from the caller, not
    /// derived from the filesystem — distinguishes "review hasn't
    /// started" from "review crashed before its first write".
    pub expected: u32,
}

/// Read filesystem state for one iteration.
///
/// `repo_id` and `target` pass through as identity for downstream
/// rendering/keying — observe never derives them. All reads are
/// scoped to `batch_dir`; no subprocess, no network. Subprocess
/// spawning lives in act.
pub(crate) fn fetch_all(
    repo_id: RepoId,
    target: ReviewTarget,
    batch_dir: &Path,
    level: CodexReasoningLevel,
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
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\ncodex\nLooks good\n").unwrap();
        fs::write(dir.join("low-2.exit"), "0\n").unwrap();

        let obs = fetch_all(
            dummy_repo_id(),
            ReviewTarget::Uncommitted,
            &dir,
            CodexReasoningLevel::Low,
            2,
        )
        .unwrap();

        assert_eq!(obs.repo_id.as_str(), "ooda-codex-review-deadbeef");
        assert_eq!(obs.target, ReviewTarget::Uncommitted);
        assert_eq!(obs.current_level, CodexReasoningLevel::Low);
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
            CodexReasoningLevel::High,
            5,
        )
        .unwrap();
        assert_eq!(obs.batch_state, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }
}
