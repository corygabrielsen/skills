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
use std::time::{Duration, SystemTime};

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
    /// In flight. `pending_slots` describes each slot still
    /// streaming, including its log mtime so decide can apply
    /// alive/idle discrimination. `completed_verdicts` carries
    /// the partial verdict set for the slots that have already
    /// finished — used both for normal progress display and for
    /// partial-batch projection when the loop must abandon
    /// pending slots (idle past threshold, or cap reached).
    Running {
        pending_slots: Vec<PendingSlot>,
        completed_verdicts: Vec<VerdictRecord>,
    },
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

/// A slot whose subprocess is still streaming — `.exit` not yet
/// present. Carries the log mtime so decide can apply alive/idle
/// discrimination (a slot whose log advanced within the last
/// [`ALIVE_THRESHOLD`] is making forward progress and must not
/// be abandoned; a slot quiet past the threshold is hung and is
/// safe to abandon).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PendingSlot {
    /// 1-indexed slot within the batch; matches the filename's
    /// slot component.
    pub slot: u32,
    /// Last write to this slot's `.log`, as seen by `scan_batch`.
    /// Serialized as the duration since the UNIX epoch so the
    /// on-disk shape is stable across hosts.
    #[serde(with = "system_time_secs")]
    pub log_mtime: SystemTime,
    /// Size of the slot's `.log` in bytes — useful for diagnostic
    /// rendering ("slot 5: streaming, 2.4 MB so far").
    pub log_bytes: u64,
}

/// Pending-slot liveness threshold. A slot whose log mtime is
/// within this window of `now` is considered alive — decide keeps
/// polling. A slot quiet for longer is hung and may be abandoned.
///
/// Empirical calibration: across observed xhigh runs, the longest
/// gap between log line timestamps within a single in-flight slot
/// was ≤60 seconds even during the codex CLI's quietest reasoning
/// phases. 90 seconds is a comfortable margin above the observed
/// p99 quiet-gap; longer thresholds delay halting genuinely hung
/// slots without buying real safety, shorter thresholds risk
/// false-abandonment of slow-but-progressing slots.
pub(crate) const ALIVE_THRESHOLD: Duration = Duration::from_secs(90);

/// Serde shim for [`SystemTime`]: store as seconds-since-epoch so
/// the recorder blob is stable across hosts (and serialization
/// never fails on a clock-pre-epoch fixture).
mod system_time_secs {
    use serde::{Serialize, Serializer};
    use std::time::SystemTime;

    pub(crate) fn serialize<S: Serializer>(t: &SystemTime, ser: S) -> Result<S::Ok, S::Error> {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        secs.serialize(ser)
    }
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
    let mut pending_slots: Vec<PendingSlot> = Vec::new();
    for (&slot, p) in &log_paths {
        let body_text = fs::read_to_string(p)?;
        let extracted = verdict::extract_verdict(&body_text);
        // Completion requires the sibling `.exit` file: the
        // subprocess has stopped writing. Without it the log may
        // be mid-stream and reading the partial body produces
        // false-cleans via substring-matched classification.
        let Some(exit_path) = exit_paths.get(&slot) else {
            // Pending: capture log mtime + size for the
            // alive/idle discriminator in decide.
            let meta = fs::metadata(p)?;
            pending_slots.push(PendingSlot {
                slot,
                log_mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                log_bytes: meta.len(),
            });
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
        std::cmp::Ordering::Less => Ok(BatchState::Running {
            pending_slots,
            completed_verdicts: verdicts,
        }),
    }
}

impl BatchState {
    /// Test-only constructor: build a `Running` variant from a
    /// (completed, total) pair. Pending slots are synthesized with
    /// "now" mtimes (always alive) so tests that exercise the
    /// pre-discriminator path continue to read as in-flight; tests
    /// that need the idle case construct slots explicitly via
    /// [`Self::running_with_idle_pending`].
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn running_alive(completed: u32, total: u32) -> Self {
        let pending_count = total.saturating_sub(completed);
        let pending_slots = (1..=pending_count)
            .map(|i| PendingSlot {
                slot: completed + i,
                log_mtime: SystemTime::now(),
                log_bytes: 0,
            })
            .collect();
        let completed_verdicts = (1..=completed)
            .map(|slot| VerdictRecord {
                slot,
                body: "test verdict".to_string(),
                class: VerdictClass::Clean,
            })
            .collect();
        Self::Running {
            pending_slots,
            completed_verdicts,
        }
    }

    /// Project a [`Self::Running`] state to a synthetic [`Self::Complete`]
    /// by abandoning any pending slots — used by decide when the
    /// alive/idle discriminator decides all pending slots are hung,
    /// and by the runner's cap-reached path when there is no further
    /// budget. Pending slots produce [`VerdictClass::Abandoned`]
    /// verdicts whose body explains the reason; the existing decide
    /// path then takes the synthetic Complete state through the
    /// normal `all_clean → DoneFixedPoint` / `AddressBatch` fork
    /// (and Abandoned counts as not-clean, so a partial sample
    /// never silently claims fixed point).
    ///
    /// Returns `None` if `self` is not `Running`; the caller is
    /// expected to dispatch on the variant first.
    pub(crate) fn project_abandoning_pending(&self, reason: &str) -> Option<Self> {
        let Self::Running {
            pending_slots,
            completed_verdicts,
        } = self
        else {
            return None;
        };
        let mut verdicts = completed_verdicts.clone();
        for p in pending_slots {
            verdicts.push(VerdictRecord {
                slot: p.slot,
                body: format!(
                    "Slot abandoned ({reason}). Last log mtime: {} epoch-seconds; \
                     log bytes streamed: {}.",
                    p.log_mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                    p.log_bytes
                ),
                class: VerdictClass::Abandoned,
            });
        }
        verdicts.sort_by_key(|v| v.slot);
        Some(Self::Complete { verdicts })
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
        match s {
            BatchState::Running {
                pending_slots,
                completed_verdicts,
            } => {
                assert_eq!(pending_slots.len(), 3);
                assert!(completed_verdicts.is_empty());
            }
            other => panic!("expected Running, got {other:?}"),
        }
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
        match s {
            BatchState::Running {
                pending_slots,
                completed_verdicts,
            } => {
                assert_eq!(pending_slots.len(), 2);
                assert_eq!(completed_verdicts.len(), 1);
                assert_eq!(completed_verdicts[0].slot, 1);
            }
            other => panic!("expected Running, got {other:?}"),
        }
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
        match s {
            BatchState::Running {
                pending_slots,
                completed_verdicts,
            } => {
                assert_eq!(pending_slots.len(), 3);
                assert!(completed_verdicts.is_empty());
            }
            other => panic!("expected Running, got {other:?}"),
        }
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
