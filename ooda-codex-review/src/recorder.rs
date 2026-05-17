//! Always-on local recorder for review-loop runs.
//!
//! # Layout
//!
//! ```text
//! <state-root>/
//!   <repo-id>/
//!     <target-key>/                    target_root()
//!       runs/
//!         <run-id>/                    current_run_dir()
//!           manifest.json
//!           levels/
//!             level-<L>/
//!               batch-<n>/             batch_dir()
//!                 <L>-<slot>.log
//!                 <L>-<slot>.exit
//!       latest                         pointer file → <run-id>
//! ```
//!
//! `<run-id>` is a sortable, collision-resistant timestamp+pid
//! triple. Parallel invocations against the same target collide
//! on the state-dir lock, not on run-ids.
//!
//! # Resume invariant
//!
//! [`Recorder::open`] resumes the run named by `latest` iff its
//! manifest's `start_level` matches the current invocation's. Any
//! other condition (missing/dangling pointer, unreadable manifest,
//! level mismatch, `cfg.fresh = true`) creates a fresh run. The
//! returned [`OpenMode`] reports which path was taken.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::decide::action::CodexReasoningLevel;
use crate::ids::{RepoId, ReviewMode, ReviewTarget};

#[derive(Debug)]
pub(crate) enum RecorderError {
    Io(io::Error),
    Serde(serde_json::Error),
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "recorder io: {e}"),
            Self::Serde(e) => write!(f, "recorder serde: {e}"),
        }
    }
}

impl std::error::Error for RecorderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Serde(e) => Some(e),
        }
    }
}

impl From<io::Error> for RecorderError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for RecorderError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RecorderConfig {
    pub state_root: PathBuf,
    pub repo_id: RepoId,
    pub target: ReviewTarget,
    /// Floor level for this invocation. Recorded as the manifest's
    /// `start_level`; mismatch on resume forces a fresh run.
    pub start_level: CodexReasoningLevel,
    /// Configured batch fan-out. Recorded in the manifest so the
    /// next invocation polls against the same expected width.
    pub batch_size: u32,
    /// When true, skip the resume probe and create a fresh run
    /// unconditionally.
    pub fresh: bool,
    /// Optional clock override for deterministic run-ids in tests.
    pub now: Option<DateTime<Utc>>,
}

/// Per-run metadata persisted as `<run_dir>/manifest.json`.
///
/// Level fields encode position on the reasoning ladder:
///
/// * `start_level` — the floor; also the resume key that must
///   match across invocations to reuse this run.
/// * `current_level` — live ladder rung; mutated by the
///   advance / drop / restart transitions.
/// * `level_history` — append-only audit log of per-level outcomes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RunManifest {
    pub run_id: String,
    pub repo_id: String,
    pub mode: ReviewMode,
    pub target_key: String,
    pub start_level: CodexReasoningLevel,
    pub current_level: CodexReasoningLevel,
    pub batch_size: u32,
    /// Batch counter at `current_level`. Level transitions select
    /// the next unused batch number for the destination, so
    /// revisiting a level never rereads stale logs.
    pub batch_number: u32,
    /// Per-level outcome history, append-only, in chronological
    /// order.
    #[serde(default)]
    pub level_history: Vec<LevelOutcome>,
    pub created_at: String,
}

/// One entry in `level_history`. Recorded by the orchestrator at
/// each per-level handoff completion. `Clean` denotes a verified
/// per-level fixed point; `Addressed` denotes a batch whose
/// issues were resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LevelOutcome {
    Clean {
        level: CodexReasoningLevel,
    },
    Addressed {
        level: CodexReasoningLevel,
        issue_count: u32,
    },
    /// Retrospective surfaced architectural patterns invalidating
    /// the prior per-level fixed points; the loop will restart
    /// from floor next.
    RetrospectiveChanges {
        level: CodexReasoningLevel,
        reason: String,
    },
}

#[derive(Debug)]
pub(crate) struct Recorder {
    target_root: PathBuf,
    current_run_dir: PathBuf,
    manifest: RunManifest,
    /// Advisory exclusive lock at `<target_root>/.lock`, held for
    /// the lifetime of the recorder. Released on drop and on FD
    /// close (including SIGKILL), so a crashed process never leaves
    /// a poisoned lock for subsequent runs.
    _lock: File,
}

/// Result of the open-time resume probe. Surfaces *why* a fresh
/// run was created so callers can log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// A new run directory was created. The wrapped reason
    /// classifies which path of the resume probe failed.
    Fresh(FreshReason),
    /// An existing run was resumed; its manifest matched the
    /// resume invariant.
    Resumed,
}

/// Discriminator for why open chose Fresh over Resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreshReason {
    /// Caller forced a fresh run.
    Forced,
    /// No `latest` pointer (first invocation for this target).
    NoLatestPointer,
    /// `latest` pointed at a run dir that no longer exists.
    LatestDangling,
    /// Manifest at the pointed-to run was missing or unparseable.
    ManifestUnreadable,
    /// Manifest's `start_level` differed from the current
    /// invocation's; per-target `latest` tracks one ladder at a
    /// time.
    LevelMismatch,
}

impl Recorder {
    /// Open a recorder for this invocation. Probes the resume
    /// pointer when `cfg.fresh` is false; falls back to a fresh
    /// run otherwise.
    pub(crate) fn open(cfg: &RecorderConfig) -> Result<(Self, OpenMode), RecorderError> {
        let target_root = compute_target_root(&cfg.state_root, &cfg.repo_id, &cfg.target);
        fs::create_dir_all(&target_root)?;

        // Advisory exclusive lock on the target dir: a concurrent
        // open against the same (repo, target) fails loudly rather
        // than corrupting the shared manifest.
        let lock_path = target_root.join(".lock");
        let lock = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        if let Err(e) = lock.try_lock() {
            return Err(RecorderError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another invocation holds the state-dir lock at {} ({e}); concurrent ooda-codex-review runs against the same (repo, target) are not supported — wait for the prior run to exit, or use --state-root to isolate",
                    lock_path.display()
                ),
            )));
        }

        if !cfg.fresh
            && let Some((run_dir, manifest, mode)) = try_resume(&target_root, cfg)?
        {
            let recorder = Self {
                target_root,
                current_run_dir: run_dir,
                manifest,
                _lock: lock,
            };
            return Ok((recorder, mode));
        }

        let fresh_reason = if cfg.fresh {
            FreshReason::Forced
        } else {
            // try_resume returned None — surface the diagnosis.
            classify_resume_failure(&target_root)
        };

        let now = cfg.now.unwrap_or_else(Utc::now);
        let id = run_id(now);
        let current_run_dir = target_root.join("runs").join(&id);
        fs::create_dir_all(&current_run_dir)?;

        let manifest = RunManifest {
            run_id: id.clone(),
            repo_id: cfg.repo_id.as_str().to_string(),
            mode: cfg.target.mode(),
            target_key: cfg.target.path_key(),
            start_level: cfg.start_level,
            current_level: cfg.start_level,
            batch_size: cfg.batch_size,
            batch_number: 1,
            level_history: Vec::new(),
            created_at: now.to_rfc3339(),
        };

        let recorder = Self {
            target_root,
            current_run_dir,
            manifest,
            _lock: lock,
        };
        recorder.write_manifest()?;
        recorder.write_latest_pointer()?;
        Ok((recorder, OpenMode::Fresh(fresh_reason)))
    }

    /// Directory holding the current level's current batch's
    /// per-slot log and exit files. See the module docs for the
    /// full layout.
    pub(crate) fn batch_dir(&self) -> PathBuf {
        self.level_dir(self.manifest.current_level)
            .join(format!("batch-{}", self.manifest.batch_number))
    }

    /// Record a per-level outcome and persist. Append-only.
    pub(crate) fn record_outcome(&mut self, outcome: LevelOutcome) -> Result<(), RecorderError> {
        self.manifest.level_history.push(outcome);
        self.write_manifest()
    }

    /// Climb one rung; idempotent at ceiling (returns `None`).
    /// Selects the next unused batch number at the destination
    /// and persists.
    pub(crate) fn advance_level(&mut self) -> Result<Option<CodexReasoningLevel>, RecorderError> {
        let Some(next) = self.manifest.current_level.higher() else {
            return Ok(None);
        };
        self.manifest.current_level = next;
        self.manifest.batch_number = self.next_batch_number_for(next)?;
        self.write_manifest()?;
        Ok(Some(next))
    }

    /// Drop one rung, clamped at floor (`start_level`); idempotent
    /// at floor (returns `None`). Selects the next unused batch
    /// number at the destination and persists.
    pub(crate) fn drop_level(&mut self) -> Result<Option<CodexReasoningLevel>, RecorderError> {
        let Some(next) = self.manifest.current_level.lower() else {
            return Ok(None);
        };
        if next < self.manifest.start_level {
            // Already at floor — clamp.
            return Ok(None);
        }
        self.manifest.current_level = next;
        self.manifest.batch_number = self.next_batch_number_for(next)?;
        self.write_manifest()?;
        Ok(Some(next))
    }

    /// Reset `current_level` to floor (`start_level`) and persist.
    /// Used when a retrospective invalidates prior per-level
    /// fixed points.
    pub(crate) fn restart_from_floor(&mut self) -> Result<CodexReasoningLevel, RecorderError> {
        self.manifest.current_level = self.manifest.start_level;
        self.manifest.batch_number = self.next_batch_number_for(self.manifest.start_level)?;
        self.write_manifest()?;
        Ok(self.manifest.start_level)
    }

    /// Hold the current level but advance to a fresh batch. Used
    /// when there is no lower level to drop to but the
    /// just-addressed batch must not be re-observed.
    pub(crate) fn start_next_batch_at_current_level(&mut self) -> Result<u32, RecorderError> {
        let next = self.next_batch_number_for(self.manifest.current_level)?;
        self.manifest.batch_number = next;
        self.write_manifest()?;
        Ok(next)
    }

    #[cfg(test)]
    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    #[cfg(test)]
    pub(crate) fn current_run_dir(&self) -> &Path {
        &self.current_run_dir
    }

    pub(crate) fn manifest(&self) -> &RunManifest {
        &self.manifest
    }

    fn write_manifest(&self) -> Result<(), RecorderError> {
        // Atomic + durable: a torn write would silently collapse
        // the next resume probe to Fresh and lose level_history.
        let path = self.current_run_dir.join("manifest.json");
        let bytes = serde_json::to_vec_pretty(&self.manifest)?;
        ooda_core::atomic_io::write_atomic(&path, &bytes)?;
        Ok(())
    }

    fn write_latest_pointer(&self) -> Result<(), RecorderError> {
        // Plain text run-id, not a symlink: portable across
        // filesystems that don't support symlinks and trivially
        // inspectable. Atomic + durable: a truncated pointer
        // breaks resume entirely.
        let path = self.target_root.join("latest");
        ooda_core::atomic_io::write_atomic(&path, self.manifest.run_id.as_bytes())?;
        Ok(())
    }

    fn level_dir(&self, level: CodexReasoningLevel) -> PathBuf {
        self.current_run_dir
            .join("levels")
            .join(format!("level-{}", level.as_str()))
    }

    fn next_batch_number_for(&self, level: CodexReasoningLevel) -> Result<u32, RecorderError> {
        let level_dir = self.level_dir(level);
        let read_dir = match fs::read_dir(&level_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(1),
            Err(e) => return Err(e.into()),
        };

        let mut max_seen = 0;
        for entry in read_dir {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(raw) = name.strip_prefix("batch-") else {
                continue;
            };
            let Ok(n) = raw.parse::<u32>() else {
                continue;
            };
            max_seen = max_seen.max(n);
        }
        Ok(max_seen.saturating_add(1).max(1))
    }
}

fn compute_target_root(state_root: &Path, repo_id: &RepoId, target: &ReviewTarget) -> PathBuf {
    state_root.join(repo_id.as_str()).join(target.path_key())
}

/// Probe the resume pointer. Returns `None` for any condition
/// that fails the resume invariant; the caller then falls back to
/// a fresh run and disambiguates via [`classify_resume_failure`].
fn try_resume(
    target_root: &Path,
    cfg: &RecorderConfig,
) -> Result<Option<(PathBuf, RunManifest, OpenMode)>, RecorderError> {
    let latest = target_root.join("latest");
    let id = match fs::read_to_string(&latest) {
        Ok(s) => s.trim().to_string(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if id.is_empty() {
        return Ok(None);
    }
    let run_dir = target_root.join("runs").join(&id);
    if !run_dir.is_dir() {
        return Ok(None);
    }
    let manifest_path = run_dir.join("manifest.json");
    let Ok(manifest_bytes) = fs::read(&manifest_path) else {
        return Ok(None);
    };
    let manifest: RunManifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if manifest.start_level != cfg.start_level {
        return Ok(None);
    }
    Ok(Some((run_dir, manifest, OpenMode::Resumed)))
}

/// After [`try_resume`] returned `None`, re-walk the same checks
/// to pick the most-specific `FreshReason`. Bounded I/O on a few
/// small files.
fn classify_resume_failure(target_root: &Path) -> FreshReason {
    let latest = target_root.join("latest");
    let id = match fs::read_to_string(&latest) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return FreshReason::NoLatestPointer,
    };
    if id.is_empty() {
        return FreshReason::NoLatestPointer;
    }
    let run_dir = target_root.join("runs").join(&id);
    if !run_dir.is_dir() {
        return FreshReason::LatestDangling;
    }
    let manifest_path = run_dir.join("manifest.json");
    let Ok(bytes) = fs::read(&manifest_path) else {
        return FreshReason::ManifestUnreadable;
    };
    if serde_json::from_slice::<RunManifest>(&bytes).is_err() {
        return FreshReason::ManifestUnreadable;
    }
    // Manifest parsed but rejected: level mismatch is the only
    // current rejection rule, so any remaining failure is that.
    FreshReason::LevelMismatch
}

fn run_id(now: DateTime<Utc>) -> String {
    format!(
        "{}-{:09}-p{}",
        now.format("%Y%m%dT%H%M%SZ"),
        now.timestamp_subsec_nanos(),
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::BranchName;

    // ─── RunManifest schema goldens ────────────────────────────────
    //
    // The on-disk contract is `RunManifest` serialized as JSON.
    // Field names and the shape of `level_history` entries are
    // part of the resume invariant: a rename must surface as a
    // test failure.
    //
    // The match in `level_outcome_golden` is exhaustive over
    // `LevelOutcome`; adding a new variant fails to compile until
    // a golden arm is added.

    fn sample_manifest_with_history(history: Vec<LevelOutcome>) -> RunManifest {
        RunManifest {
            run_id: "run-deadbeef".into(),
            repo_id: "repo-cafebabe".into(),
            mode: ReviewMode::Uncommitted,
            target_key: "uncommitted".into(),
            start_level: CodexReasoningLevel::Low,
            current_level: CodexReasoningLevel::Medium,
            batch_size: 3,
            batch_number: 2,
            level_history: history,
            created_at: "2026-05-15T10:00:00Z".into(),
        }
    }

    fn level_outcome_golden(o: &LevelOutcome) -> serde_json::Value {
        use serde_json::json;
        match o {
            LevelOutcome::Clean { level } => json!({
                "kind": "clean",
                "level": level,
            }),
            LevelOutcome::Addressed { level, issue_count } => json!({
                "kind": "addressed",
                "level": level,
                "issue_count": issue_count,
            }),
            LevelOutcome::RetrospectiveChanges { level, reason } => json!({
                "kind": "retrospective_changes",
                "level": level,
                "reason": reason,
            }),
        }
    }

    fn manifest_golden(m: &RunManifest) -> serde_json::Value {
        use serde_json::json;
        json!({
            "run_id": m.run_id,
            "repo_id": m.repo_id,
            "mode": m.mode,
            "target_key": m.target_key,
            "start_level": m.start_level,
            "current_level": m.current_level,
            "batch_size": m.batch_size,
            "batch_number": m.batch_number,
            "level_history": m.level_history.iter().map(level_outcome_golden).collect::<Vec<_>>(),
            "created_at": m.created_at,
        })
    }

    /// One sample `LevelOutcome` per variant. Hand-maintained; the
    /// length sentinel in `manifest_schema_goldens_exhaustive`
    /// catches drift.
    fn level_outcome_samples() -> Vec<LevelOutcome> {
        vec![
            LevelOutcome::Clean {
                level: CodexReasoningLevel::Low,
            },
            LevelOutcome::Addressed {
                level: CodexReasoningLevel::Medium,
                issue_count: 3,
            },
            LevelOutcome::RetrospectiveChanges {
                level: CodexReasoningLevel::High,
                reason: "extract helper".into(),
            },
        ]
    }

    /// Exhaustive snapshot test for the `RunManifest` JSON shape —
    /// the on-disk schema other tools and the resume-probe code
    /// depend on.
    #[test]
    fn manifest_schema_goldens_exhaustive() {
        let samples = level_outcome_samples();
        assert_eq!(
            samples.len(),
            3,
            "`level_outcome_samples` must include one sample per `LevelOutcome` variant; \
             adding a new variant requires adding both a golden arm in `level_outcome_golden` \
             AND a sample here.",
        );
        // One manifest with every history-entry variant present, so
        // a single round-trip exercises both the manifest fields and
        // every `LevelOutcome` arm.
        let manifest = sample_manifest_with_history(samples);
        let actual: serde_json::Value = serde_json::to_value(&manifest).unwrap();
        let expected = manifest_golden(&manifest);
        assert_eq!(actual, expected, "RunManifest schema mismatch");
    }

    fn temp_state_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ooda-codex-review-recorder-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dummy_cfg(state_root: PathBuf) -> RecorderConfig {
        RecorderConfig {
            state_root,
            repo_id: RepoId::parse("repo-deadbeef0001").unwrap(),
            target: ReviewTarget::Uncommitted,
            start_level: CodexReasoningLevel::Low,
            batch_size: 3,
            fresh: false,
            now: Some(
                DateTime::parse_from_rfc3339("2026-05-03T10:00:00.000000123Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        }
    }

    #[test]
    fn target_root_layout_is_state_repo_target() {
        let id = RepoId::parse("repo-abc123").unwrap();
        let r = compute_target_root(Path::new("/state"), &id, &ReviewTarget::Uncommitted);
        assert_eq!(r, PathBuf::from("/state/repo-abc123/uncommitted"));

        let b = BranchName::parse("master").unwrap();
        let r = compute_target_root(Path::new("/state"), &id, &ReviewTarget::Base(b));
        assert_eq!(r, PathBuf::from("/state/repo-abc123/base/master"));
    }

    #[test]
    fn open_creates_run_dir_and_writes_manifest() {
        let root = temp_state_root("open");
        let (rec, mode) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        assert_eq!(mode, OpenMode::Fresh(FreshReason::NoLatestPointer));
        assert!(rec.current_run_dir().exists(), "run dir must exist");
        let manifest_path = rec.current_run_dir().join("manifest.json");
        assert!(manifest_path.exists(), "manifest.json must exist");

        let bytes = fs::read(&manifest_path).unwrap();
        let parsed: RunManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.repo_id, "repo-deadbeef0001");
        assert_eq!(parsed.mode, ReviewMode::Uncommitted);
        assert_eq!(parsed.target_key, "uncommitted");
        assert_eq!(parsed.start_level, CodexReasoningLevel::Low);
        assert_eq!(parsed.current_level, CodexReasoningLevel::Low);
        assert_eq!(parsed.batch_size, 3);
        assert_eq!(parsed.batch_number, 1);
        assert!(parsed.level_history.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn open_writes_latest_pointer_with_run_id() {
        let root = temp_state_root("latest-pointer");
        let (rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        let latest = rec.target_root().join("latest");
        let id_from_pointer = fs::read_to_string(&latest).unwrap();
        assert_eq!(id_from_pointer, rec.manifest().run_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn run_id_format_includes_timestamp_nanos_and_pid() {
        let t = DateTime::parse_from_rfc3339("2026-05-03T10:00:00.000000123Z")
            .unwrap()
            .with_timezone(&Utc);
        let id = run_id(t);
        assert!(id.starts_with("20260503T100000Z-000000123-p"));
    }

    #[test]
    fn batch_dir_includes_run_id_level_and_batch_number() {
        let root = temp_state_root("batch-dir");
        let (rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        let bd = rec.batch_dir();
        let expected_suffix = format!("runs/{}/levels/level-low/batch-1", rec.manifest().run_id);
        assert!(
            bd.ends_with(&expected_suffix),
            "batch_dir = {bd:?}, expected suffix {expected_suffix}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // ----- Phase 8b resume scenarios ----------------------------------

    #[test]
    fn second_open_resumes_same_run_when_target_and_level_match() {
        let root = temp_state_root("resume-hit");
        let (first, m1) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        let first_id = first.manifest().run_id.clone();
        assert_eq!(m1, OpenMode::Fresh(FreshReason::NoLatestPointer));
        // Drop the first recorder to release its state-dir lock before
        // re-opening — production callers always exit between invocations.
        drop(first);

        // Bump the clock so a fresh run would produce a different
        // run-id; resume must ignore the clock.
        let mut cfg = dummy_cfg(root.clone());
        cfg.now = Some(
            DateTime::parse_from_rfc3339("2026-05-04T10:00:00.000000123Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let (second, m2) = Recorder::open(&cfg).unwrap();
        assert_eq!(m2, OpenMode::Resumed);
        assert_eq!(second.manifest().run_id, first_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_flag_forces_new_run_even_with_valid_latest() {
        let root = temp_state_root("resume-fresh-forced");
        let (first, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        let first_id = first.manifest().run_id.clone();
        drop(first);

        let mut cfg = dummy_cfg(root.clone());
        cfg.fresh = true;
        cfg.now = Some(
            DateTime::parse_from_rfc3339("2026-05-04T10:00:00.000000123Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let (second, mode) = Recorder::open(&cfg).unwrap();
        assert_eq!(mode, OpenMode::Fresh(FreshReason::Forced));
        assert_ne!(second.manifest().run_id, first_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn level_mismatch_forces_fresh_run() {
        let root = temp_state_root("resume-level-mismatch");
        let (first, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        drop(first);

        let mut cfg = dummy_cfg(root.clone());
        cfg.start_level = CodexReasoningLevel::High;
        cfg.now = Some(
            DateTime::parse_from_rfc3339("2026-05-04T10:00:00.000000123Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let (rec, mode) = Recorder::open(&cfg).unwrap();
        assert_eq!(mode, OpenMode::Fresh(FreshReason::LevelMismatch));
        assert_eq!(rec.manifest().start_level, CodexReasoningLevel::High);

        let _ = fs::remove_dir_all(&root);
    }

    /// A second open while a first recorder is still alive must fail
    /// loudly — concurrent writes to the same manifest would corrupt
    /// it. Dropping the first releases the lock and the second open
    /// then succeeds normally.
    #[test]
    fn second_open_blocks_while_first_recorder_alive() {
        let root = temp_state_root("lock-blocks-concurrent");
        let (first, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        let blocked = Recorder::open(&dummy_cfg(root.clone()));
        assert!(
            matches!(blocked, Err(RecorderError::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock),
            "expected WouldBlock from concurrent open, got {blocked:?}"
        );
        drop(first);
        let (resumed, mode) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        assert_eq!(mode, OpenMode::Resumed);
        assert!(resumed.current_run_dir().is_dir());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dangling_latest_pointer_is_classified_and_recovered() {
        let root = temp_state_root("resume-dangling");
        // Pre-create a `latest` pointer whose run dir doesn't exist.
        let target_root = root
            .join("repo-deadbeef0001")
            .join(ReviewTarget::Uncommitted.path_key());
        fs::create_dir_all(&target_root).unwrap();
        fs::write(target_root.join("latest"), "ghost-run-id").unwrap();

        let (rec, mode) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        assert_eq!(mode, OpenMode::Fresh(FreshReason::LatestDangling));
        assert!(rec.current_run_dir().is_dir());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_manifest_is_classified_and_recovered() {
        let root = temp_state_root("resume-bad-manifest");
        let target_root = root
            .join("repo-deadbeef0001")
            .join(ReviewTarget::Uncommitted.path_key());
        let bad_run = target_root.join("runs").join("bad-run");
        fs::create_dir_all(&bad_run).unwrap();
        fs::write(bad_run.join("manifest.json"), b"not json{").unwrap();
        fs::write(target_root.join("latest"), "bad-run").unwrap();

        let (_rec, mode) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        assert_eq!(mode, OpenMode::Fresh(FreshReason::ManifestUnreadable));

        let _ = fs::remove_dir_all(&root);
    }

    // ----- Phase 6b ladder mutations ----------------------------------

    #[test]
    fn advance_level_climbs_one_rung_and_persists() {
        let root = temp_state_root("advance");
        let (mut rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Low);

        let next = rec.advance_level().unwrap();
        assert_eq!(next, Some(CodexReasoningLevel::Medium));
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Medium);
        assert_eq!(rec.manifest().batch_number, 1);

        // Disk reflects the new level.
        let bytes = fs::read(rec.current_run_dir().join("manifest.json")).unwrap();
        let parsed: RunManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.current_level, CodexReasoningLevel::Medium);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn advance_level_at_ceiling_returns_none() {
        let root = temp_state_root("advance-ceiling");
        let mut cfg = dummy_cfg(root.clone());
        cfg.start_level = CodexReasoningLevel::Xhigh;
        let (mut rec, _) = Recorder::open(&cfg).unwrap();

        assert_eq!(rec.advance_level().unwrap(), None);
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Xhigh);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drop_level_clamps_at_floor() {
        let root = temp_state_root("drop-clamp");
        let mut cfg = dummy_cfg(root.clone());
        cfg.start_level = CodexReasoningLevel::Medium;
        let (mut rec, _) = Recorder::open(&cfg).unwrap();

        // Climb so we have somewhere to drop to.
        rec.advance_level().unwrap();
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::High);

        // Drop back to floor.
        let dropped = rec.drop_level().unwrap();
        assert_eq!(dropped, Some(CodexReasoningLevel::Medium));
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Medium);

        // Already at floor — drop is a no-op.
        assert_eq!(rec.drop_level().unwrap(), None);
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Medium);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restart_from_floor_resets_to_start_level() {
        let root = temp_state_root("restart");
        let mut cfg = dummy_cfg(root.clone());
        cfg.start_level = CodexReasoningLevel::Low;
        let (mut rec, _) = Recorder::open(&cfg).unwrap();

        rec.advance_level().unwrap(); // medium
        rec.advance_level().unwrap(); // high
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::High);

        let restarted = rec.restart_from_floor().unwrap();
        assert_eq!(restarted, CodexReasoningLevel::Low);
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Low);
        assert_eq!(rec.manifest().batch_number, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_outcome_appends_and_persists() {
        let root = temp_state_root("record-outcome");
        let (mut rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        rec.record_outcome(LevelOutcome::Clean {
            level: CodexReasoningLevel::Low,
        })
        .unwrap();
        rec.record_outcome(LevelOutcome::Addressed {
            level: CodexReasoningLevel::Medium,
            issue_count: 4,
        })
        .unwrap();

        assert_eq!(rec.manifest().level_history.len(), 2);

        // Round-trip through disk.
        let bytes = fs::read(rec.current_run_dir().join("manifest.json")).unwrap();
        let parsed: RunManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.level_history.len(), 2);
        assert!(matches!(
            parsed.level_history[0],
            LevelOutcome::Clean {
                level: CodexReasoningLevel::Low
            }
        ));
        assert!(matches!(
            parsed.level_history[1],
            LevelOutcome::Addressed {
                level: CodexReasoningLevel::Medium,
                issue_count: 4
            }
        ));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_dir_uses_current_level_after_advance() {
        let root = temp_state_root("batch-dir-advance");
        let (mut rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();
        rec.advance_level().unwrap();

        let bd = rec.batch_dir();
        let expected_suffix = format!("runs/{}/levels/level-medium/batch-1", rec.manifest().run_id);
        assert!(
            bd.ends_with(&expected_suffix),
            "batch_dir = {bd:?}, expected suffix {expected_suffix}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn revisiting_a_level_uses_next_unused_batch_number() {
        let root = temp_state_root("batch-dir-revisit");
        let (mut rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        rec.advance_level().unwrap(); // medium, batch 1
        fs::create_dir_all(rec.batch_dir()).unwrap();
        rec.advance_level().unwrap(); // high
        rec.drop_level().unwrap(); // medium again

        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Medium);
        assert_eq!(rec.manifest().batch_number, 2);
        let expected_suffix = format!("runs/{}/levels/level-medium/batch-2", rec.manifest().run_id);
        assert!(rec.batch_dir().ends_with(&expected_suffix));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restart_from_floor_uses_next_unused_floor_batch() {
        let root = temp_state_root("restart-next-batch");
        let (mut rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        fs::create_dir_all(rec.batch_dir()).unwrap(); // low/batch-1 exists
        rec.advance_level().unwrap(); // medium
        rec.advance_level().unwrap(); // high
        rec.restart_from_floor().unwrap();

        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Low);
        assert_eq!(rec.manifest().batch_number, 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn can_start_next_batch_without_changing_level() {
        let root = temp_state_root("same-level-next-batch");
        let (mut rec, _) = Recorder::open(&dummy_cfg(root.clone())).unwrap();

        fs::create_dir_all(rec.batch_dir()).unwrap(); // low/batch-1 exists
        let next = rec.start_next_batch_at_current_level().unwrap();

        assert_eq!(next, 2);
        assert_eq!(rec.manifest().current_level, CodexReasoningLevel::Low);
        assert_eq!(rec.manifest().batch_number, 2);

        let _ = fs::remove_dir_all(&root);
    }
}
