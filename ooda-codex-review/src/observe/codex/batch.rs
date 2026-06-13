//! Per-level batch scanning: read the run directory, count log
//! files, extract completed verdicts, project to [`BatchState`].
//!
//! # Filesystem layout
//!
//! ```text
//! <batch_dir>/
//!   {level}-{slot}.log
//!   {level}-{slot}.exit
//! ```
//!
//! # Completion predicate
//!
//! A log is "completed" iff (a) its sibling `.exit` file is
//! present (the subprocess has exited) AND (b) the log contains
//! the verdict marker AND (c) a non-empty body after the marker.
//! The `.exit` file is the ground truth that the subprocess has
//! stopped writing; without it, marker+body may be mid-stream and
//! reading the partial body produces false-cleans via substring-
//! matched verdict classification.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::decide::action::CodexReasoningLevel;

use super::verdict::{self, VerdictClass};

/// Per-level batch state from the observe layer's perspective.
/// The four values partition the filesystem signal completely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum BatchState {
    /// No log files for this level. The spawn has not happened —
    /// or happened and failed before any subprocess created its
    /// log.
    NotStarted,
    /// In flight. `total` files exist; `completed` of them have
    /// satisfied the completion predicate; the remainder are
    /// streaming.
    Running { total: u32, completed: u32 },
    /// `expected` files have all completed; per-slot bodies and
    /// classifications attached.
    Complete { verdicts: Vec<VerdictRecord> },
    /// More completed slots observed than `expected`. Indicates a
    /// stale-state condition (e.g., stray log from a prior run,
    /// off-by-one between caller's `n` and observer's `expected`).
    /// Returning [`Self::Running`] from this shape would silently
    /// emit `AwaitReviews { pending: 0 }`, a Wait that the loop
    /// honours forever; surface to a human resolver instead.
    InconsistentState {
        total: u32,
        completed: u32,
        expected: u32,
        reason: String,
    },
}

/// One reviewer's verdict within a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VerdictRecord {
    /// 1-indexed slot within the batch; matches the filename's
    /// slot component.
    pub slot: u32,
    /// Raw body — everything after the last verdict marker line.
    pub body: String,
    /// Heuristic classification.
    pub class: VerdictClass,
}

/// Scan `batch_dir` and project to a [`BatchState`].
///
/// Decision table over (file count, completion count, expected):
///
/// | files | completed | result |
/// |-------|-----------|--------|
/// | 0     | —         | `NotStarted` |
/// | n > 0 | c < expected | `Running { total = n, completed = c }` |
/// | n > 0 | c == expected | `Complete { verdicts }` |
/// | n > 0 | c > expected | `InconsistentState { .. }` |
///
/// `expected` is required because filesystem absence alone cannot
/// distinguish "spawn hasn't happened" from "spawn happened and
/// crashed before any first write".
pub(crate) fn scan_batch(
    batch_dir: &Path,
    level: CodexReasoningLevel,
    expected: u32,
) -> io::Result<BatchState> {
    let prefix = format!("{}-", level.as_str());
    let read_dir = match fs::read_dir(batch_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BatchState::NotStarted),
        Err(e) => return Err(e),
    };

    // Per-batch-dir advisory lock. Acquired cooperatively against
    // the spawn-side lock that excludes log truncation; the
    // observe pass thus never reads a half-truncated log mid
    // re-spawn.
    let _batch_lock = ooda_core::FileLock::acquire(&batch_dir.join(".batch.lock"))?;

    let mut log_paths: BTreeMap<u32, PathBuf> = BTreeMap::new();
    let mut exit_paths: BTreeMap<u32, PathBuf> = BTreeMap::new();

    for entry in read_dir.filter_map(std::result::Result::ok) {
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
    for (&slot, p) in &log_paths {
        let body_text = fs::read_to_string(p)?;
        let extracted = verdict::extract_verdict(&body_text);
        // Completion requires the sibling `.exit` file: the
        // subprocess has stopped writing. Without it the log may
        // be mid-stream and reading the partial body produces
        // false-cleans via substring-matched classification.
        let Some(exit_path) = exit_paths.get(&slot) else {
            continue;
        };
        let code = read_exit_status(exit_path)?;
        if code != 0 {
            return Err(io::Error::other(format!(
                "codex review slot {slot} exited {code}; see {}",
                p.display()
            )));
        }
        let verdict_body = match extracted {
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
            Some(body) => body,
        };
        verdicts.push(VerdictRecord {
            slot,
            class: verdict::classify(&verdict_body),
            body: verdict_body,
        });
    }

    // Bounded by the per-iteration reviewer fan-out; u32 fits.
    let total = u32::try_from(log_paths.len()).expect("batch log count fits in u32");
    let completed = u32::try_from(verdicts.len()).expect("batch verdict count fits in u32");
    match completed.cmp(&expected) {
        std::cmp::Ordering::Equal => Ok(BatchState::Complete { verdicts }),
        std::cmp::Ordering::Greater => Ok(BatchState::InconsistentState {
            total,
            completed,
            expected,
            reason: format!(
                "completed slots ({completed}) exceed expected fan-out ({expected}) in {}; \
                 stray log file or mismatched caller `n`",
                batch_dir.display()
            ),
        }),
        std::cmp::Ordering::Less => Ok(BatchState::Running { total, completed }),
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
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
        assert_eq!(s, BatchState::NotStarted);
    }

    #[test]
    fn empty_dir_is_not_started() {
        let dir = temp_batch_dir("empty");
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_other_levels() {
        let dir = temp_batch_dir("other-levels");
        fs::write(dir.join("high-1.log"), "thinking\ncodex\nverdict\n").unwrap();
        fs::write(dir.join("medium-1.log"), "thinking\ncodex\nverdict\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
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
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
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
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\n").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
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
    fn marker_plus_body_without_exit_file_is_running() {
        // Regression: a log with the verdict marker plus a single
        // post-marker byte but no `.exit` file is the codex
        // subprocess mid-stream. Classifying it as Complete races
        // the partial body into the verdict and can produce a
        // false-clean via substring matching.
        let dir = temp_batch_dir("mid-stream");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nN").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\ncodex\nL").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\ncodex\nR").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
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
    fn full_completion_classifies_each() {
        let dir = temp_batch_dir("complete");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();
        fs::write(
            dir.join("low-2.log"),
            "thinking\ncodex\nReview comment: src/foo.rs:42\n",
        )
        .unwrap();
        fs::write(dir.join("low-2.exit"), "0\n").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\ncodex\nLooks good.\n").unwrap();
        fs::write(dir.join("low-3.exit"), "0\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
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

        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1).unwrap_err();
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

        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1).unwrap_err();
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

        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("without a verdict body"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_exit_status_is_binary_error() {
        let dir = temp_batch_dir("orphan-exit");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.exit"), "0\n").unwrap();

        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("slot 2"), "msg: {msg}");
        assert!(msg.contains("without a matching log"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_slots_keep_filename_slot_numbers() {
        let dir = temp_batch_dir("filename-slots");
        fs::write(dir.join("low-2.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.exit"), "0\n").unwrap();

        let s = scan_batch(&dir, CodexReasoningLevel::Low, 1).unwrap();
        match s {
            BatchState::Complete { verdicts } => assert_eq!(verdicts[0].slot, 2),
            other => panic!("expected Complete, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_completed_logs_surface_as_inconsistent_state() {
        // 4 done but expected=3 — stray log from a prior batch or
        // an n-mismatch between the caller and the observer. Used
        // to project to Running { completed=4, total=4 }, which
        // decide turned into AwaitReviews { pending: 0 } — a Wait
        // the loop honours forever. Now surfaces as
        // InconsistentState so a human resolver can intervene.
        let dir = temp_batch_dir("oversize");
        for n in 1..=4 {
            fs::write(
                dir.join(format!("low-{n}.log")),
                "thinking\ncodex\nNo issues found\n",
            )
            .unwrap();
            fs::write(dir.join(format!("low-{n}.exit")), "0\n").unwrap();
        }
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3).unwrap();
        match s {
            BatchState::InconsistentState {
                total,
                completed,
                expected,
                reason,
            } => {
                assert_eq!(total, 4);
                assert_eq!(completed, 4);
                assert_eq!(expected, 3);
                assert!(reason.contains("exceed expected"), "reason: {reason}");
            }
            other => panic!("expected InconsistentState, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
