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
//!   {level}-1.exit
//!   {level}-2.exit
//!   ...
//!   {level}-n.exit
//! ```
//!
//! A log file is "completed" once it contains a `codex` marker
//! line AND a non-empty body after the marker. The marker-only
//! state (body still streaming) counts as Running — operationally
//! the body lands within seconds of the marker, but observing
//! mid-stream once burned us with empty-verdict false-cleans.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

    let mut log_paths: BTreeMap<u32, PathBuf> = BTreeMap::new();
    let mut exit_paths: BTreeMap<u32, PathBuf> = BTreeMap::new();

    for entry in read_dir.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(slot) = parse_slot(&name, &prefix, ".log") {
            log_paths.insert(slot, path);
        } else if let Some(slot) = parse_slot(&name, &prefix, ".exit") {
            exit_paths.insert(slot, path);
        }
    }

    if log_paths.is_empty() && exit_paths.is_empty() {
        return Ok(BatchState::NotStarted);
    }
    if log_paths.is_empty() {
        return Err(io::Error::other(format!(
            "codex review wrote exit status without a log in {}; cannot classify batch",
            batch_dir.display()
        )));
    }
    if let Some(slot) = exit_paths
        .keys()
        .find(|slot| !log_paths.contains_key(slot))
        .copied()
    {
        return Err(io::Error::other(format!(
            "codex review slot {slot} wrote exit status without a matching log in {}; cannot classify batch",
            batch_dir.display()
        )));
    }

    let mut verdicts = Vec::with_capacity(log_paths.len());
    for (&slot, p) in log_paths.iter() {
        let body_text = fs::read_to_string(p)?;
        let extracted = verdict::extract_verdict(&body_text);
        if let Some(exit_path) = exit_paths.get(&slot) {
            let code = read_exit_status(exit_path)?;
            if code != 0 {
                return Err(io::Error::other(format!(
                    "codex review slot {slot} exited {code}; see {}",
                    p.display()
                )));
            }
            match extracted.as_ref() {
                None => {
                    return Err(io::Error::other(format!(
                        "codex review slot {slot} exited 0 without a verdict marker; see {}",
                        p.display()
                    )));
                }
                Some(body) if body.trim().is_empty() => {
                    return Err(io::Error::other(format!(
                        "codex review slot {slot} exited 0 without a verdict body; see {}",
                        p.display()
                    )));
                }
                Some(_) => {}
            }
        }
        if let Some(verdict_body) = extracted.filter(|b| !b.trim().is_empty()) {
            verdicts.push(VerdictRecord {
                slot,
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

fn parse_slot(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    let raw = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if raw.is_empty() || raw.starts_with('0') {
        return None;
    }
    raw.parse::<u32>().ok().filter(|slot| *slot > 0)
}

fn read_exit_status(path: &Path) -> io::Result<i32> {
    let raw = fs::read_to_string(path)?;
    raw.trim().parse::<i32>().map_err(|e| {
        io::Error::other(format!(
            "invalid codex review exit status in {}: {e}",
            path.display()
        ))
    })
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
    fn nonzero_exit_status_is_binary_error_instead_of_running_forever() {
        let dir = temp_batch_dir("nonzero-exit");
        fs::write(dir.join("low-1.log"), "error: unexpected argument '--pr'\n").unwrap();
        fs::write(dir.join("low-1.exit"), "2\n").unwrap();

        let err = scan_batch(&dir, ReasoningLevel::Low, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("slot 1 exited 2"), "msg: {msg}");
        assert!(msg.contains("low-1.log"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_exit_without_verdict_marker_is_binary_error() {
        let dir = temp_batch_dir("zero-no-marker");
        fs::write(dir.join("low-1.log"), "thinking\nfinished without marker\n").unwrap();
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();

        let err = scan_batch(&dir, ReasoningLevel::Low, 1).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exited 0 without a verdict marker"),
            "msg: {msg}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_exit_with_empty_verdict_body_is_binary_error() {
        let dir = temp_batch_dir("zero-empty-body");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\n").unwrap();
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();

        let err = scan_batch(&dir, ReasoningLevel::Low, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("without a verdict body"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_exit_status_is_binary_error() {
        let dir = temp_batch_dir("orphan-exit");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.exit"), "0\n").unwrap();

        let err = scan_batch(&dir, ReasoningLevel::Low, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("slot 2"), "msg: {msg}");
        assert!(msg.contains("without a matching log"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_slots_keep_filename_slot_numbers() {
        let dir = temp_batch_dir("filename-slots");
        fs::write(dir.join("low-2.log"), "thinking\ncodex\nNo issues found\n").unwrap();

        let s = scan_batch(&dir, ReasoningLevel::Low, 1).unwrap();
        match s {
            BatchState::Complete { verdicts } => assert_eq!(verdicts[0].slot, 2),
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
