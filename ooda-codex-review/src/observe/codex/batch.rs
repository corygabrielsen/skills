//! Per-level batch scanning: read the run directory, count log
//! files, extract completed verdicts, build [`BatchState`].
//!
//! Filesystem layout (per Phase 8 plan):
//!
//! ```text
//! <batch_dir>/
//!   {level}-1.log
//!   {level}-2.log
//!   ...
//!   {level}-n.log
//! ```
//!
//! A log file is "completed" once it contains a `codex` marker
//! line AND a non-empty body after the marker. The marker-only
//! state (body still streaming) counts as Running — operationally
//! the body lands within seconds of the marker, but observing
//! mid-stream once burned us with empty-verdict false-cleans.

use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use crate::decide::action::ReasoningLevel;

use super::verdict::{self, VerdictClass};

/// Per-level batch state. The three discrete states a batch can
/// be in from the observe layer's perspective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BatchState {
    /// No log files for this level yet — `RunReviews` has not
    /// been dispatched (or its spawn failed before any process
    /// created its log).
    NotStarted,
    /// Some logs are still streaming. `total` files exist on
    /// disk; `completed` of them have a verdict body extracted.
    /// `pending = total - completed`. Decide reads `completed`
    /// vs `expected` to choose `AwaitReviews` vs `ParseVerdicts`.
    Running { total: u32, completed: u32 },
    /// All `expected` log files have completed verdicts. The
    /// per-slot bodies and classifications are attached for the
    /// orient/decide layers.
    Complete { verdicts: Vec<VerdictRecord> },
}

/// One reviewer's verdict within a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerdictRecord {
    /// 1-indexed slot within the batch (matches the `{n}` in
    /// `{level}-{n}.log`).
    pub slot: u32,
    /// Raw verdict body — everything after the last `codex`
    /// marker line in the log.
    pub body: String,
    /// Heuristic classification.
    pub class: VerdictClass,
}

/// Scan `batch_dir` for `{level}-*.log` files and produce a
/// [`BatchState`]. `expected` is the configured `n` — the number
/// of reviews launched for this batch.
///
/// Three-way logic:
///   - 0 files                 → `NotStarted`
///   - completed < expected    → `Running { total, completed }`
///   - completed == expected   → `Complete { verdicts }`
///
/// `expected` is required because filesystem absence cannot
/// distinguish "review hasn't started" from "review crashed
/// before its first log write". The orient/decide layer owns
/// that interpretation; observe surfaces what's on disk.
pub fn scan_batch(
    batch_dir: &Path,
    level: ReasoningLevel,
    expected: u32,
) -> io::Result<BatchState> {
    let prefix = format!("{}-", level.as_str());
    let read_dir = match fs::read_dir(batch_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BatchState::NotStarted),
        Err(e) => return Err(e),
    };

    let mut log_paths: Vec<_> = read_dir
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with(&prefix) && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();
    log_paths.sort();

    if log_paths.is_empty() {
        return Ok(BatchState::NotStarted);
    }

    let mut verdicts = Vec::with_capacity(log_paths.len());
    for (idx, p) in log_paths.iter().enumerate() {
        let body_text = fs::read_to_string(p)?;
        if let Some(verdict_body) =
            verdict::extract_verdict(&body_text).filter(|b| !b.trim().is_empty())
        {
            verdicts.push(VerdictRecord {
                slot: (idx + 1) as u32,
                class: verdict::classify(&verdict_body),
                body: verdict_body,
            });
        }
    }

    let total = log_paths.len() as u32;
    let completed = verdicts.len() as u32;
    if completed == expected {
        Ok(BatchState::Complete { verdicts })
    } else {
        Ok(BatchState::Running { total, completed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_batch_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ooda-codex-review-batch-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn missing_dir_is_not_started() {
        let dir = std::env::temp_dir().join(format!(
            "ooda-codex-review-batch-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        assert_eq!(s, BatchState::NotStarted);
    }

    #[test]
    fn empty_dir_is_not_started() {
        let dir = temp_batch_dir("empty");
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_other_levels() {
        let dir = temp_batch_dir("other-levels");
        fs::write(dir.join("high-1.log"), "thinking\ncodex\nverdict\n").unwrap();
        fs::write(dir.join("medium-1.log"), "thinking\ncodex\nverdict\n").unwrap();
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_only_counts_as_running() {
        let dir = temp_batch_dir("marker-only");
        // Marker present but body empty → still streaming.
        fs::write(dir.join("low-1.log"), "thinking\ncodex\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\n").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\n").unwrap();
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        assert_eq!(
            s,
            BatchState::Running {
                total: 3,
                completed: 0
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_completion_is_running() {
        let dir = temp_batch_dir("partial");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\n").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\n").unwrap();
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        assert_eq!(
            s,
            BatchState::Running {
                total: 3,
                completed: 1
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_completion_classifies_each() {
        let dir = temp_batch_dir("complete");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(
            dir.join("low-2.log"),
            "thinking\ncodex\nReview comment: src/foo.rs:42\n",
        )
        .unwrap();
        fs::write(dir.join("low-3.log"), "thinking\ncodex\nLooks good.\n").unwrap();
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        match s {
            BatchState::Complete { verdicts } => {
                assert_eq!(verdicts.len(), 3);
                assert_eq!(verdicts[0].slot, 1);
                assert_eq!(verdicts[0].class, VerdictClass::Clean);
                assert_eq!(verdicts[1].slot, 2);
                assert_eq!(verdicts[1].class, VerdictClass::HasIssues);
                assert_eq!(verdicts[2].slot, 3);
                assert_eq!(verdicts[2].class, VerdictClass::Clean);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_completed_logs_are_still_running_until_expected_match() {
        // 4 done but expected=3 — could happen if a stray log lingers from
        // a prior batch. Treat as Running so decide doesn't commit to a
        // mis-sized verdict set.
        let dir = temp_batch_dir("oversize");
        for n in 1..=4 {
            fs::write(
                dir.join(format!("low-{n}.log")),
                "thinking\ncodex\nNo issues found\n",
            )
            .unwrap();
        }
        let s = scan_batch(&dir, ReasoningLevel::Low, 3).unwrap();
        assert_eq!(
            s,
            BatchState::Running {
                total: 4,
                completed: 4
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
