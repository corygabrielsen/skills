//! Scan a single batch directory and reduce it to a [`BatchState`].
//!
//! # Batch layout
//!
//! A *batch* is a directory containing:
//! - an *identity stamp* file recording the target identity the
//!   batch was spawned against,
//! - per-slot *log* files holding reviewer output, named
//!   `{level}-{slot}.log`,
//! - optional per-slot *exit* files holding subprocess exit codes,
//!   named `{level}-{slot}.exit`.
//!
//! # Invariants
//!
//! - **Slot completion ⇔ marker + non-empty body**: a log is
//!   *completed* when its verdict marker is present AND the body
//!   following it is non-empty. Marker-without-body is a streaming
//!   intermediate, not a clean verdict — collapsing them would
//!   admit empty-body false-cleans.
//! - **Identity gate**: a batch directory whose stamp is missing
//!   or whose recorded identity does not match the supplied one is
//!   reported as not-started, forcing re-spawn. This is the entire
//!   mechanism by which a target-identity change invalidates prior
//!   reviewer batches.
//! - **Exit-without-log is a binary error**: an exit file with no
//!   matching log indicates the spawn protocol was violated; the
//!   scan cannot classify the batch and fails loudly rather than
//!   silently dropping the slot.
//! - **Zero-exit-without-verdict is a binary error**: a slot that
//!   exited successfully but produced no verdict marker or an empty
//!   body fails the scan — a clean exit must produce a verdict.
//! - **`completed == expected` is required for `Complete`**: extra
//!   completed slots (stragglers from a prior batch) keep the
//!   batch in `Running` so decide does not commit to a mis-sized
//!   verdict set.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ids::CodexReasoningLevel;

use super::verdict::{self, VerdictClass};

/// Discrete state of a batch from the observe layer's perspective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum BatchState {
    /// No batch is in progress for the current target identity.
    /// Either no slots have been spawned, or the on-disk batch was
    /// stamped against a different identity and is being ignored.
    NotStarted,
    /// `total` slots have logs on disk; `completed ≤ total` of
    /// them have a non-empty verdict body. The remainder are
    /// streaming.
    Running { total: u32, completed: u32 },
    /// `expected` slots have produced verdict bodies. Per-slot
    /// records are attached.
    Complete { verdicts: Vec<VerdictRecord> },
}

/// A single reviewer's verdict within a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VerdictRecord {
    /// 1-indexed slot identifier within the batch.
    pub slot: u32,
    /// Verdict body — the log suffix after the last verdict marker.
    pub body: String,
    /// Classification of the body.
    pub class: VerdictClass,
}

/// Reduce a batch directory to a [`BatchState`] against the
/// supplied target identity.
///
/// **Reduction**:
/// - Identity-stamp mismatch or absence → `NotStarted`.
/// - Empty directory → `NotStarted`.
/// - `completed < expected` → `Running { total, completed }`.
/// - `completed == expected` → `Complete { verdicts }`.
///
/// **Errors**: protocol violations (exit without log, zero-exit
/// without verdict, non-zero exit) surface as `io::Error::other`
/// rather than silently degrading the classification.
///
/// `expected` is supplied externally because filesystem absence
/// cannot distinguish "not yet spawned" from "spawned and crashed
/// pre-write"; the calling layer owns that interpretation.
pub(crate) fn scan_batch(
    batch_dir: &Path,
    level: CodexReasoningLevel,
    expected: u32,
    expected_head_sha: &str,
) -> io::Result<BatchState> {
    let prefix = format!("{}-", level.as_str());
    let read_dir = match fs::read_dir(batch_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BatchState::NotStarted),
        Err(e) => return Err(e),
    };

    // Identity gate — see module invariant.
    match fs::read_to_string(batch_dir.join("head_sha.txt")) {
        Ok(stored) => {
            if stored.trim() != expected_head_sha {
                return Ok(BatchState::NotStarted);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BatchState::NotStarted),
        Err(e) => return Err(e),
    }

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

    // Batch fan-out is bounded; widths fit in u32.
    let total = u32::try_from(log_paths.len()).expect("batch log count fits in u32");
    let completed = u32::try_from(verdicts.len()).expect("batch verdict count fits in u32");
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

    const SHA: &str = "matchsha";

    fn temp_batch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ooda-pr-codex-review-batch-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_head(dir: &Path, sha: &str) {
        fs::write(dir.join("head_sha.txt"), sha).unwrap();
    }

    #[test]
    fn missing_dir_is_not_started() {
        let dir = std::env::temp_dir().join(format!(
            "ooda-pr-codex-review-batch-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
        assert_eq!(s, BatchState::NotStarted);
    }

    #[test]
    fn empty_dir_with_head_is_not_started() {
        let dir = temp_batch_dir("empty");
        write_head(&dir, SHA);
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_without_head_sha_is_not_started() {
        let dir = temp_batch_dir("no-head-sha");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        // No head_sha.txt → treat as never started.
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 1, "abc").unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_sha_mismatch_is_not_started() {
        let dir = temp_batch_dir("head-sha-mismatch");
        write_head(&dir, "old-sha");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 1, "new-sha").unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_other_levels() {
        let dir = temp_batch_dir("other-levels");
        write_head(&dir, SHA);
        fs::write(dir.join("high-1.log"), "thinking\ncodex\nverdict\n").unwrap();
        fs::write(dir.join("medium-1.log"), "thinking\ncodex\nverdict\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_only_counts_as_running() {
        let dir = temp_batch_dir("marker-only");
        write_head(&dir, SHA);
        // Marker without body must classify as streaming, not clean.
        fs::write(dir.join("low-1.log"), "thinking\ncodex\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\n").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
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
        write_head(&dir, SHA);
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.log"), "thinking\n").unwrap();
        fs::write(dir.join("low-3.log"), "thinking\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
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
    fn full_completion_with_matching_head_classifies_each() {
        let dir = temp_batch_dir("complete");
        write_head(&dir, SHA);
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(
            dir.join("low-2.log"),
            "thinking\ncodex\nReview comment: src/foo.rs:42\n",
        )
        .unwrap();
        fs::write(dir.join("low-3.log"), "thinking\ncodex\nLooks good.\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
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
        write_head(&dir, SHA);
        fs::write(dir.join("low-1.log"), "error: unexpected argument '--pr'\n").unwrap();
        fs::write(dir.join("low-1.exit"), "2\n").unwrap();
        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1, SHA).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("slot 1 exited 2"), "msg: {msg}");
        assert!(msg.contains("low-1.log"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_exit_without_verdict_marker_is_binary_error() {
        let dir = temp_batch_dir("zero-no-marker");
        write_head(&dir, SHA);
        fs::write(dir.join("low-1.log"), "thinking\nfinished without marker\n").unwrap();
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();
        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1, SHA).unwrap_err();
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
        write_head(&dir, SHA);
        fs::write(dir.join("low-1.log"), "thinking\ncodex\n").unwrap();
        fs::write(dir.join("low-1.exit"), "0\n").unwrap();
        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1, SHA).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("without a verdict body"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_exit_status_is_binary_error() {
        let dir = temp_batch_dir("orphan-exit");
        write_head(&dir, SHA);
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(dir.join("low-2.exit"), "0\n").unwrap();
        let err = scan_batch(&dir, CodexReasoningLevel::Low, 1, SHA).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("slot 2"), "msg: {msg}");
        assert!(msg.contains("without a matching log"), "msg: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_slots_keep_filename_slot_numbers() {
        let dir = temp_batch_dir("filename-slots");
        write_head(&dir, SHA);
        fs::write(dir.join("low-2.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 1, SHA).unwrap();
        match s {
            BatchState::Complete { verdicts } => assert_eq!(verdicts[0].slot, 2),
            other => panic!("expected Complete, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_completed_logs_are_still_running_until_expected_match() {
        // completed > expected must NOT classify Complete — see
        // module invariant on exact-match completion.
        let dir = temp_batch_dir("oversize");
        write_head(&dir, SHA);
        for n in 1..=4 {
            fs::write(
                dir.join(format!("low-{n}.log")),
                "thinking\ncodex\nNo issues found\n",
            )
            .unwrap();
        }
        let s = scan_batch(&dir, CodexReasoningLevel::Low, 3, SHA).unwrap();
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
