//! Per-level batch scanning: read the run directory, count log
//! files, extract completed verdicts, build [`BatchState`].
//!
//! Layout:
//! ```text
//! <batch_dir>/
//!   head_sha.txt              (the PR head SHA the batch was spawned against)
//!   {level}-1.log             stdout/stderr of the codex review subprocess
//!   {level}-1.exit            exit status when the subprocess finished
//!   ...
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ids::ReasoningLevel;

use super::verdict::{self, VerdictClass};

/// Per-level batch state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BatchState {
    NotStarted,
    Running { total: u32, completed: u32 },
    Complete { verdicts: Vec<VerdictRecord> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerdictRecord {
    pub slot: u32,
    pub body: String,
    pub class: VerdictClass,
}

/// Scan `batch_dir` for `{level}-*.log` files and produce a
/// [`BatchState`]. Stale batches (mismatched `head_sha.txt`) are
/// reported as [`BatchState::NotStarted`] so the runner re-spawns
/// at the current head.
pub fn scan_batch(
    batch_dir: &Path,
    level: ReasoningLevel,
    expected: u32,
    expected_head_sha: &str,
) -> io::Result<BatchState> {
    let prefix = format!("{}-", level.as_str());
    let read_dir = match fs::read_dir(batch_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BatchState::NotStarted),
        Err(e) => return Err(e),
    };

    // Head SHA gate: a batch_dir without head_sha.txt or with a
    // mismatch is treated as if the batch never started so the
    // runner re-spawns at the current head.
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
        let s = scan_batch(&dir, ReasoningLevel::Low, 3, "deadbeef").unwrap();
        assert_eq!(s, BatchState::NotStarted);
    }

    #[test]
    fn dir_without_head_sha_is_not_started() {
        let dir = temp_batch_dir("no-head-sha");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        // No head_sha.txt → treat as never started.
        let s = scan_batch(&dir, ReasoningLevel::Low, 1, "abc").unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_sha_mismatch_is_not_started() {
        let dir = temp_batch_dir("head-sha-mismatch");
        write_head(&dir, "old-sha");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        let s = scan_batch(&dir, ReasoningLevel::Low, 1, "new-sha").unwrap();
        assert_eq!(s, BatchState::NotStarted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_completion_with_matching_head_classifies_each() {
        let dir = temp_batch_dir("complete");
        write_head(&dir, "matchsha");
        fs::write(dir.join("low-1.log"), "thinking\ncodex\nNo issues found\n").unwrap();
        fs::write(
            dir.join("low-2.log"),
            "thinking\ncodex\nReview comment: src/foo.rs:42\n",
        )
        .unwrap();
        fs::write(dir.join("low-3.log"), "thinking\ncodex\nLooks good.\n").unwrap();
        let s = scan_batch(&dir, ReasoningLevel::Low, 3, "matchsha").unwrap();
        match s {
            BatchState::Complete { verdicts } => {
                assert_eq!(verdicts.len(), 3);
                assert_eq!(verdicts[0].class, VerdictClass::Clean);
                assert_eq!(verdicts[1].class, VerdictClass::HasIssues);
                assert_eq!(verdicts[2].class, VerdictClass::Clean);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
