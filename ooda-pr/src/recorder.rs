//! Always-on PR memory harness.
//!
//! # Role
//!
//! Sole persistence boundary for the per-PR loop. Runtime code
//! reports events (observations, decisions, tool calls, actions,
//! comments, waits, outcomes) via this module's API; all on-disk
//! representations are owned here.
//!
//! # Invariants
//!
//! - **Single writer per PR**: one `Recorder` per `(slug, pr)`;
//!   internal mutation serialized by `Arc<Mutex<_>>`.
//! - **Append-only event log**: per-iteration events monotonic in
//!   `sequence`; existing records are never rewritten.
//! - **Immutable per-iteration artifacts**: each artifact is
//!   content-addressed; the pointer manifest (`CURRENT.json`) is
//!   the only mutable file at the PR root.
//! - **Atomic pointer publication**: every write to a stable
//!   read-surface file flows through `write_atomic` so concurrent
//!   readers observe pre- or post-state, never a tear.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::Decision;
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::orient::OrientedState;
use crate::outcome::Outcome;
use ooda_core::{CURRENT_MANIFEST_SCHEMA_VERSION, CurrentManifest, ExitCode};

const SCHEMA_VERSION: u32 = 1;

static PROCESS_RECORDER: OnceLock<Mutex<Option<Recorder>>> = OnceLock::new();

#[derive(Debug)]
pub(crate) enum RecorderError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RecorderError {}

impl From<io::Error> for RecorderError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for RecorderError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunMode {
    Loop,
    Inspect,
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loop => f.write_str("loop"),
            Self::Inspect => f.write_str("inspect"),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RecorderConfig {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub mode: RunMode,
    pub max_iter: std::num::NonZeroU32,
    pub status_comment: bool,
    pub state_root: Option<PathBuf>,
    pub legacy_trace: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArtifactRef {
    pub path: String,
    pub blob: String,
    pub sha256: String,
    pub bytes: usize,
    pub media_type: String,
}

#[derive(Clone)]
pub(crate) struct Recorder {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    slug: RepoSlug,
    pr: PullRequestNumber,
    mode: RunMode,
    max_iter: std::num::NonZeroU32,
    status_comment: bool,
    state_root: PathBuf,
    pr_root: PathBuf,
    run_root: PathBuf,
    run_id: String,
    sequence: u64,
    tool_sequence: u64,
    current_iteration: Option<u32>,
    events: File,
    run_events: File,
    trace_md: File,
    legacy_trace: Option<File>,
}

#[derive(Serialize)]
struct EventRecord<'a> {
    schema_version: u32,
    sequence: u64,
    timestamp: DateTime<Utc>,
    run_id: &'a str,
    iteration: Option<u32>,
    kind: &'a str,
    summary: String,
    artifacts: Vec<ArtifactRef>,
    data: Value,
}

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    run_id: &'a str,
    started_at: DateTime<Utc>,
    forge: &'a str,
    repo: &'a RepoSlug,
    pr: PullRequestNumber,
    mode: RunMode,
    max_iter: std::num::NonZeroU32,
    status_comment: bool,
    cwd: String,
    argv: Vec<String>,
}

#[derive(Serialize)]
struct ToolCallRecord<'a> {
    call_id: &'a str,
    program: &'a str,
    args: Vec<String>,
    cwd: String,
    duration_ms: u128,
    status_code: Option<i32>,
    success: bool,
}

#[derive(Serialize)]
struct ToolCallFailure<'a> {
    call_id: &'a str,
    program: &'a str,
    args: Vec<String>,
    cwd: String,
    duration_ms: u128,
    error: String,
}

#[derive(Serialize, Deserialize)]
struct EventRange {
    first_sequence: u64,
    last_sequence: u64,
}

impl Recorder {
    pub(crate) fn open(cfg: RecorderConfig) -> Result<Self, RecorderError> {
        let state_root =
            ooda_core::state_root::resolve_ooda_pr_state_root(cfg.state_root.as_deref());
        let pr_root = pull_request_root(&state_root, &cfg.slug, cfg.pr);
        let now = Utc::now();
        let run_id = run_id(now);
        let run_root = pr_root.join("runs").join(&run_id);

        fs::create_dir_all(run_root.join("iterations"))?;
        fs::create_dir_all(pr_root.join("status-comment"))?;
        fs::create_dir_all(pr_root.join("blobs").join("sha256"))?;

        let events = append_file(&pr_root.join("events.jsonl"))?;
        let run_events = append_file(&run_root.join("trace.jsonl"))?;
        let trace_md = append_file(&run_root.join("trace.md"))?;
        let legacy_trace = match cfg.legacy_trace.as_deref() {
            Some(path) => Some(append_file(path)?),
            None => None,
        };

        let recorder = Self {
            inner: Arc::new(Mutex::new(Inner {
                slug: cfg.slug,
                pr: cfg.pr,
                mode: cfg.mode,
                max_iter: cfg.max_iter,
                status_comment: cfg.status_comment,
                state_root,
                pr_root,
                run_root,
                run_id,
                sequence: 0,
                tool_sequence: 0,
                current_iteration: None,
                events,
                run_events,
                trace_md,
                legacy_trace,
            })),
        };

        recorder.initialize(now)?;
        Ok(recorder)
    }

    pub(crate) fn install_process_recorder(&self) {
        let cell = PROCESS_RECORDER.get_or_init(|| Mutex::new(None));
        if let Ok(mut slot) = cell.lock() {
            *slot = Some(self.clone());
        }
    }

    pub(crate) fn set_iteration(&self, iteration: Option<u32>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.current_iteration = iteration;
        }
    }

    #[cfg(test)]
    pub(crate) fn pull_request_root(&self) -> PathBuf {
        self.with_inner(|inner| inner.pr_root.clone())
            .unwrap_or_default()
    }

    pub(crate) fn dedup_path(&self) -> PathBuf {
        self.with_inner(|inner| inner.pr_root.join("status-comment").join("dedup.json"))
            .unwrap_or_default()
    }

    /// Persist a handoff prompt body as a per-iteration artifact
    /// and return its absolute path.
    ///
    /// # Postcondition
    ///
    /// On `Some(path)`: bytes are durable, content-addressed in the
    /// blob store, and reachable from `CURRENT.json` via the
    /// deterministic derivation in `publish_current` — no
    /// cross-call state-passing is needed.
    ///
    /// # Failure modes
    ///
    /// Returns `None` when the write fails or no iteration is set.
    /// Caller must fall back to inline stderr emission so the
    /// prompt is never lost.
    ///
    /// # Invariant
    ///
    /// Decouples prompt consumption from stderr's truncation
    /// budget: the file's size is observable via `stat`, so readers
    /// have no streaming-truncation pressure.
    pub(crate) fn write_handoff_md(&self, prompt: &str) -> Option<PathBuf> {
        let mut inner = self.inner.lock().ok()?;
        let iteration = inner.current_iteration?;
        let artifact = inner
            .write_artifact_bytes(
                Some(iteration),
                "handoff.md",
                prompt.as_bytes(),
                "text/markdown",
            )
            .ok()?;
        Some(inner.pr_root.join(&artifact.path))
    }

    pub(crate) fn write_trace_line(&self, line: &str) {
        self.best_effort(|inner| {
            writeln!(inner.trace_md, "{line}")?;
            if let Some(file) = inner.legacy_trace.as_mut() {
                writeln!(file, "{line}")?;
            }
            Ok(())
        });
    }

    pub(crate) fn record_iteration<TObs>(
        &self,
        iteration: u32,
        observations: &TObs,
        oriented: &OrientedState,
        candidates: &[Action],
        decision: &Decision,
    ) where
        TObs: Serialize,
    {
        let dashboard = Dashboard::from_iteration(oriented, candidates, decision);
        self.best_effort(|inner| {
            inner.current_iteration = Some(iteration);

            let obs_ref = inner.write_json_artifact(
                Some(iteration),
                "normalized.json",
                observations,
                "application/json",
            )?;
            let oriented_ref = inner.write_json_artifact(
                Some(iteration),
                "oriented.json",
                oriented,
                "application/json",
            )?;
            let candidates_ref = inner.write_json_artifact(
                Some(iteration),
                "candidates.json",
                candidates,
                "application/json",
            )?;
            // Two projections of the same decision, distinct
            // schemas: `decision_envelope.json` is the runner's
            // typed dispatch input (Execute/Halt); `dashboard.json`
            // is the tier-grouped human-shaped projection.
            // Separable filenames so downstream consumers select
            // by schema, not by parsing.
            let decision_envelope_ref = inner.write_json_artifact(
                Some(iteration),
                "decision_envelope.json",
                decision,
                "application/json",
            )?;
            let dashboard_ref = inner.write_json_artifact(
                Some(iteration),
                "dashboard.json",
                &dashboard,
                "application/json",
            )?;

            // Markdown surfaces are written here inline; the
            // pointer manifest is derived deterministically from
            // `(run_id, iteration)` in `publish_current`. Invariant:
            // the producer and the manifest projection share no
            // mutable index — adding a surface requires editing
            // both sites, and the deterministic derivation catches
            // omissions.
            let index_md = render_iteration_index_md(
                &inner.slug,
                inner.pr,
                &inner.run_id,
                iteration,
                decision,
            );
            inner.write_artifact_bytes(
                Some(iteration),
                "index.md",
                index_md.as_bytes(),
                "text/markdown",
            )?;
            inner.write_artifact_bytes(
                Some(iteration),
                "blockers.md",
                dashboard.render_blockers_md().as_bytes(),
                "text/markdown",
            )?;
            inner.write_artifact_bytes(
                Some(iteration),
                "next.md",
                dashboard.render_next_md().as_bytes(),
                "text/markdown",
            )?;

            // `action.json` carries the decision's payload, whose
            // type depends on the variant: `Action` for `Execute`,
            // `HandoffAction` for `AgentNeeded`/`HumanNeeded`.
            // Both `Serialize`; discrimination lives in the
            // enclosing decision-projection record, not here.
            match decision {
                Decision::Execute(action) => {
                    inner.write_json_artifact(
                        Some(iteration),
                        "action.json",
                        action,
                        "application/json",
                    )?;
                }
                Decision::Halt(
                    crate::decide::decision::DecisionHalt::AgentNeeded(handoff)
                    | crate::decide::decision::DecisionHalt::HumanNeeded(handoff),
                ) => {
                    inner.write_json_artifact(
                        Some(iteration),
                        "action.json",
                        handoff,
                        "application/json",
                    )?;
                }
                Decision::Halt(_) => {}
            }

            let artifacts = vec![
                obs_ref,
                oriented_ref,
                candidates_ref,
                decision_envelope_ref,
                dashboard_ref,
            ];
            inner.append_event(
                Some(iteration),
                "iteration_decided",
                decision_summary(decision),
                artifacts,
                json!({
                    "candidate_count": candidates.len(),
                    "decision": decision_projection(decision),
                }),
            )?;
            inner.append_ledger("decision", &decision_summary(decision))?;
            Ok(())
        });
    }

    pub(crate) fn record_observe_start(&self, iteration: u32) {
        self.best_effort(|inner| {
            inner.append_event(
                Some(iteration),
                "observe_started",
                format!("iteration {iteration} observe started"),
                vec![],
                json!({}),
            )
        });
    }

    pub(crate) fn record_observe_end(&self, iteration: u32, result: Result<(), String>) {
        self.best_effort(|inner| {
            let success = result.is_ok();
            let error = result.err();
            let summary = if success {
                format!("iteration {iteration} observe succeeded")
            } else {
                format!("iteration {iteration} observe failed")
            };
            inner.append_event(
                Some(iteration),
                "observe_finished",
                summary,
                vec![],
                json!({
                    "success": success,
                    "error": error,
                }),
            )
        });
    }

    pub(crate) fn record_status_comment_rendered<T: Serialize>(
        &self,
        iteration: Option<u32>,
        rendered: &T,
        summary: impl Into<String>,
    ) {
        self.best_effort(|inner| {
            let artifact = inner.write_json_artifact(
                iteration,
                "status-comment/rendered.json",
                rendered,
                "application/json",
            )?;
            inner.append_event(
                iteration,
                "status_comment_rendered",
                summary.into(),
                vec![artifact],
                json!({}),
            )
        });
    }

    pub(crate) fn record_status_comment_result<T: Serialize>(
        &self,
        iteration: Option<u32>,
        result: &T,
        summary: impl Into<String>,
    ) {
        self.best_effort(|inner| {
            let artifact = inner.write_json_artifact(
                iteration,
                "status-comment/result.json",
                result,
                "application/json",
            )?;
            inner.append_event(
                iteration,
                "status_comment_result",
                summary.into(),
                vec![artifact],
                json!({}),
            )
        });
    }

    pub(crate) fn record_action_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.append_event(
                Some(iteration),
                "action_started",
                action_summary(action),
                vec![],
                json!({ "action": action_projection(action) }),
            )
        });
    }

    pub(crate) fn record_action_end(
        &self,
        iteration: u32,
        action: &Action,
        result: Result<(), String>,
    ) {
        self.best_effort(|inner| {
            let success = result.is_ok();
            let error = result.err();
            let summary = if success {
                format!("{} succeeded", action.kind.name())
            } else {
                format!("{} failed", action.kind.name())
            };
            let record = json!({
                "action": action_projection(action),
                "success": success,
                "error": error,
            });
            let artifact = inner.write_json_artifact(
                Some(iteration),
                "act-result.json",
                &record,
                "application/json",
            )?;
            inner.append_event(
                Some(iteration),
                "action_finished",
                summary,
                vec![artifact],
                record,
            )
        });
    }

    pub(crate) fn record_wait_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.append_event(
                Some(iteration),
                "wait_started",
                action_summary(action),
                vec![],
                json!({ "action": action_projection(action) }),
            )
        });
    }

    pub(crate) fn record_wait_end(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.append_event(
                Some(iteration),
                "wait_finished",
                action_summary(action),
                vec![],
                json!({ "action": action_projection(action) }),
            )
        });
    }

    pub(crate) fn record_outcome(
        &self,
        outcome: &Outcome,
        code: ExitCode,
        headline: &str,
        handoff_path: Option<&Path>,
    ) {
        self.best_effort(|inner| {
            let outcome_ref =
                inner.write_json_artifact(None, "outcome.json", outcome, "application/json")?;
            inner.append_event(
                inner.current_iteration,
                "outcome",
                outcome_summary(outcome, code),
                vec![outcome_ref],
                json!({ "exit_code": code }),
            )?;
            inner.append_ledger("outcome", &outcome_summary(outcome, code))?;
            writeln!(inner.trace_md, "exit={code}")?;
            if let Some(file) = inner.legacy_trace.as_mut() {
                writeln!(file, "exit={code}")?;
            }
            inner.publish_current(outcome, code, headline, handoff_path)?;
            Ok(())
        });
    }

    fn initialize(&self, started_at: DateTime<Utc>) -> Result<(), RecorderError> {
        let mut inner = self.inner.lock().map_err(|_| {
            RecorderError::Io(io::Error::other("recorder lock poisoned during open"))
        })?;
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            run_id: &inner.run_id,
            started_at,
            forge: "github.com",
            repo: &inner.slug,
            pr: inner.pr,
            mode: inner.mode,
            max_iter: inner.max_iter,
            status_comment: inner.status_comment,
            cwd: std::env::current_dir()
                .map_or_else(|_| "<unknown>".to_string(), |p| p.display().to_string()),
            argv: std::env::args().collect(),
        };
        Inner::write_json_at(&inner.run_root.join("manifest.json"), &manifest)?;
        let header = format!(
            "===== ooda-pr {} repo={} pr={} mode={} max_iter={} status_comment={} state_root={} run_id={} =====",
            started_at.to_rfc3339(),
            inner.slug,
            inner.pr,
            inner.mode,
            inner.max_iter,
            inner.status_comment,
            inner.state_root.display(),
            inner.run_id,
        );
        writeln!(inner.trace_md, "{header}")?;
        if let Some(file) = inner.legacy_trace.as_mut() {
            writeln!(file, "{header}")?;
        }
        let summary = format!("run {} started", inner.run_id);
        let data = json!({
            "repo": inner.slug,
            "pr": inner.pr,
            "mode": inner.mode,
            "max_iter": inner.max_iter,
            "status_comment": inner.status_comment,
            "state_root": inner.state_root,
        });
        inner.append_event(None, "run_started", summary, vec![], data)?;
        Ok(())
    }

    fn best_effort(&self, f: impl FnOnce(&mut Inner) -> Result<(), RecorderError>) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = f(&mut inner);
        }
    }

    fn with_inner<T>(&self, f: impl FnOnce(&Inner) -> T) -> Option<T> {
        self.inner.lock().ok().map(|inner| f(&inner))
    }
}

impl Inner {
    fn append_event(
        &mut self,
        iteration: Option<u32>,
        kind: &'static str,
        summary: String,
        artifacts: Vec<ArtifactRef>,
        data: Value,
    ) -> Result<(), RecorderError> {
        self.sequence += 1;
        let event = EventRecord {
            schema_version: SCHEMA_VERSION,
            sequence: self.sequence,
            timestamp: Utc::now(),
            run_id: &self.run_id,
            iteration,
            kind,
            summary,
            artifacts,
            data,
        };
        serde_json::to_writer(&mut self.events, &event)?;
        writeln!(self.events)?;
        serde_json::to_writer(&mut self.run_events, &event)?;
        writeln!(self.run_events)?;
        if let Some(iteration) = iteration {
            self.update_event_range(iteration, self.sequence)?;
        }
        Ok(())
    }

    fn update_event_range(&self, iteration: u32, sequence: u64) -> Result<(), RecorderError> {
        let path = self
            .run_root
            .join("iterations")
            .join(format!("{iteration:04}"))
            .join("event-range.json");
        // `first_sequence` resolution forms a fallback chain over
        // the file's state:
        //   parseable  → trust its `first_sequence`.
        //   corrupt    → re-derive from the authoritative event log.
        //   absent     → this is the iteration's first event;
        //                first == last == sequence.
        // The append-only event log is the source of truth; the
        // event-range file is a derivable index.
        let first_sequence = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<EventRange>(&bytes) {
                Ok(range) => range.first_sequence,
                Err(_) => self
                    .first_sequence_for_iteration(iteration)
                    .unwrap_or(sequence),
            },
            Err(_) => sequence,
        };
        let range = EventRange {
            first_sequence,
            last_sequence: sequence,
        };
        Self::write_json_at(&path, &range)
    }

    /// Recovery projection: minimum sequence over the iteration's
    /// events in the authoritative event log. `None` when the log
    /// is unreadable, malformed, or carries no event for the
    /// iteration. The append-only log is the source of truth;
    /// derivable index files can always be rebuilt from it.
    fn first_sequence_for_iteration(&self, iteration: u32) -> Option<u64> {
        let path = self.run_root.join("trace.jsonl");
        let content = fs::read_to_string(&path).ok()?;
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let it = v.get("iteration").and_then(serde_json::Value::as_u64);
            if it.is_some_and(|n| u32::try_from(n).is_ok_and(|n| n == iteration)) {
                return v.get("sequence").and_then(serde_json::Value::as_u64);
            }
        }
        None
    }

    fn append_ledger(&mut self, kind: &str, summary: &str) -> Result<(), RecorderError> {
        let entry = json!({
            "schema_version": SCHEMA_VERSION,
            "timestamp": Utc::now(),
            "run_id": self.run_id,
            "kind": kind,
            "summary": summary,
        });
        let mut jsonl = append_file(&self.pr_root.join("ledger.jsonl"))?;
        serde_json::to_writer(&mut jsonl, &entry)?;
        writeln!(jsonl)?;

        let mut md = append_file(&self.pr_root.join("ledger.md"))?;
        writeln!(
            md,
            "- {} `{}` {}",
            Utc::now().to_rfc3339(),
            self.run_id,
            summary
        )?;
        Ok(())
    }

    fn next_tool_call_id(&mut self) -> String {
        self.tool_sequence += 1;
        format!("tc-{:06}", self.tool_sequence)
    }

    fn write_json_artifact<T: Serialize + ?Sized>(
        &mut self,
        iteration: Option<u32>,
        relative: &str,
        value: &T,
        media_type: &str,
    ) -> Result<ArtifactRef, RecorderError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        self.write_artifact_bytes(iteration, relative, &bytes, media_type)
    }

    fn write_artifact_bytes(
        &mut self,
        iteration: Option<u32>,
        relative: &str,
        bytes: &[u8],
        media_type: &str,
    ) -> Result<ArtifactRef, RecorderError> {
        let artifact_path = match iteration {
            Some(i) => self
                .run_root
                .join("iterations")
                .join(format!("{i:04}"))
                .join(relative),
            None => self.run_root.join(relative),
        };
        write_bytes_at(&artifact_path, bytes)?;

        let hash = sha256_hex(bytes);
        let blob_rel = PathBuf::from("blobs")
            .join("sha256")
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(format!("{hash}.zst"));
        let blob_abs = self.pr_root.join(&blob_rel);
        if !blob_abs.exists() {
            let compressed = zstd::stream::encode_all(bytes, 0)?;
            write_bytes_at(&blob_abs, &compressed)?;
        }

        Ok(ArtifactRef {
            path: relative_path(&self.pr_root, &artifact_path),
            blob: blob_rel.display().to_string(),
            sha256: hash,
            bytes: bytes.len(),
            media_type: media_type.to_string(),
        })
    }

    fn write_json_at<T: Serialize>(path: &Path, value: &T) -> Result<(), RecorderError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        write_bytes_at(path, &bytes)?;
        Ok(())
    }

    /// Atomically publish the pointer manifest reflecting this
    /// invocation's terminal outcome.
    ///
    /// # Invariants
    ///
    /// - **Deterministic projection**: the symbol → relative-path
    ///   map is a pure function of `(run_id, iteration,
    ///   outcome-shape, handoff_path)` — no shared mutable state
    ///   crosses the producer/projection seam.
    /// - **Conditional inclusion**:
    ///   - `action` ⇔ outcome carries an `Action`/`HandoffAction`
    ///     payload (predicate: `outcome_has_action`).
    ///   - `handoff` ⇔ `handoff_path` is `Some` (caller's
    ///     postcondition: the file is durable).
    /// - **Atomic publication**: `write_atomic` guarantees readers
    ///   observe the prior or new manifest, never a torn write.
    ///
    /// `keep_runs` is the GC retention extension point — empty by
    /// default; entries pin additional run-ids past the active run.
    fn publish_current(
        &self,
        outcome: &Outcome,
        code: ExitCode,
        headline: &str,
        handoff_path: Option<&Path>,
    ) -> Result<(), RecorderError> {
        let iteration = self.current_iteration.unwrap_or(0);
        let iter_dir = format!("runs/{}/iterations/{iteration:04}", self.run_id);
        let run_dir = format!("runs/{}", self.run_id);

        let mut artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (sym, fname) in [
            ("normalized", "normalized.json"),
            ("state", "oriented.json"),
            ("candidates", "candidates.json"),
            ("decision_envelope", "decision_envelope.json"),
            ("dashboard", "dashboard.json"),
            ("index", "index.md"),
            ("blockers", "blockers.md"),
            ("next", "next.md"),
        ] {
            artifacts.insert(
                sym.to_string(),
                PathBuf::from(format!("{iter_dir}/{fname}")),
            );
        }
        if outcome_has_action(outcome) {
            artifacts.insert(
                "action".to_string(),
                PathBuf::from(format!("{iter_dir}/action.json")),
            );
        }
        if let Some(path) = handoff_path {
            artifacts.insert(
                "handoff".to_string(),
                path.strip_prefix(&self.pr_root)
                    .map_or_else(|_| path.to_path_buf(), Path::to_path_buf),
            );
        }
        artifacts.insert(
            "outcome".to_string(),
            PathBuf::from(format!("{run_dir}/outcome.json")),
        );

        let manifest = CurrentManifest {
            schema_version: CURRENT_MANIFEST_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            iteration,
            exit_code: code.as_u8(),
            headline: headline.to_string(),
            artifacts,
            keep_runs: Vec::new(),
        };
        let body = serde_json::to_vec_pretty(&manifest)?;
        write_bytes_at(&self.pr_root.join("CURRENT.json"), &body)?;
        Ok(())
    }
}

/// Outcome → action-payload predicate. True ⇔ the per-iteration
/// `action.json` write fired for this outcome's terminal iteration.
/// The variant set here is the dual of the match in
/// `record_iteration`; the two sites are kept congruent by the
/// `Action`/`HandoffAction`-carrying invariant of these outcomes.
fn outcome_has_action(outcome: &Outcome) -> bool {
    matches!(
        outcome,
        Outcome::WouldAdvance(_)
            | Outcome::HandoffAgent(_)
            | Outcome::HandoffHuman(_)
            | Outcome::StuckRepeated(_)
            | Outcome::StuckCapReached(_)
    )
}

/// Render the human-readable iteration summary. Disjoint
/// responsibility from the pointer manifest: this surface answers
/// "what is iteration N about"; the manifest answers "where does
/// each per-iteration artifact live".
fn render_iteration_index_md(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    run_id: &str,
    iteration: u32,
    decision: &Decision,
) -> String {
    format!(
        "# ooda-pr iteration {iteration}\n\n- repo: `{slug}`\n- pr: `{pr}`\n- run: `{run_id}`\n- iteration: `{iteration}`\n- decision: `{summary}`\n",
        summary = decision_summary(decision),
    )
}

pub(crate) struct ToolCallGuard {
    recorder: Recorder,
    call_id: String,
    program: String,
    args: Vec<String>,
    cwd: String,
    started: Instant,
    iteration: Option<u32>,
}

pub(crate) fn tool_call_started(program: &str, args: &[&str]) -> Option<ToolCallGuard> {
    let recorder = process_recorder()?;
    let (call_id, iteration) = next_tool_call_id_locked(&recorder)?;
    let args_v: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let cwd = std::env::current_dir()
        .map_or_else(|_| "<unknown>".to_string(), |p| p.display().to_string());

    recorder.best_effort(|inner| {
        inner.append_event(
            iteration,
            "tool_call_started",
            format!("gh {}", args_v.join(" ")),
            vec![],
            json!({
                "call_id": call_id,
                "program": program,
                "args": args_v,
                "cwd": cwd,
            }),
        )
    });

    Some(ToolCallGuard {
        recorder,
        call_id,
        program: program.to_string(),
        args: args_v,
        cwd,
        started: Instant::now(),
        iteration,
    })
}

impl ToolCallGuard {
    pub(crate) fn finish_output(self, output: &Output) {
        let duration_ms = self.started.elapsed().as_millis();
        self.recorder.best_effort(|inner| {
            let base = format!("tool-calls/{}", self.call_id);
            let stdout_ref = inner.write_artifact_bytes(
                self.iteration,
                &format!("{base}/stdout.bin"),
                &output.stdout,
                "application/octet-stream",
            )?;
            let stderr_ref = inner.write_artifact_bytes(
                self.iteration,
                &format!("{base}/stderr.bin"),
                &output.stderr,
                "application/octet-stream",
            )?;
            let record = ToolCallRecord {
                call_id: &self.call_id,
                program: &self.program,
                args: self.args.clone(),
                cwd: self.cwd.clone(),
                duration_ms,
                status_code: output.status.code(),
                success: output.status.success(),
            };
            let record_ref = inner.write_json_artifact(
                self.iteration,
                &format!("{base}/record.json"),
                &record,
                "application/json",
            )?;
            inner.append_event(
                self.iteration,
                "tool_call_finished",
                format!(
                    "{} {} exited {}",
                    self.program,
                    self.args.join(" "),
                    output
                        .status
                        .code()
                        .map_or_else(|| "?".to_string(), |c| c.to_string())
                ),
                vec![stdout_ref, stderr_ref, record_ref],
                json!({
                    "call_id": self.call_id,
                    "success": output.status.success(),
                    "status_code": output.status.code(),
                    "duration_ms": duration_ms,
                }),
            )
        });
    }

    pub(crate) fn finish_spawn_error(self, err: &io::Error) {
        let duration_ms = self.started.elapsed().as_millis();
        self.recorder.best_effort(|inner| {
            let base = format!("tool-calls/{}", self.call_id);
            let record = ToolCallFailure {
                call_id: &self.call_id,
                program: &self.program,
                args: self.args.clone(),
                cwd: self.cwd.clone(),
                duration_ms,
                error: err.to_string(),
            };
            let record_ref = inner.write_json_artifact(
                self.iteration,
                &format!("{base}/record.json"),
                &record,
                "application/json",
            )?;
            inner.append_event(
                self.iteration,
                "tool_call_finished",
                format!("{} failed to spawn", self.program),
                vec![record_ref],
                json!({
                    "call_id": self.call_id,
                    "success": false,
                    "error": err.to_string(),
                    "duration_ms": duration_ms,
                }),
            )
        });
    }
}

fn next_tool_call_id_locked(recorder: &Recorder) -> Option<(String, Option<u32>)> {
    let mut inner = recorder.inner.lock().ok()?;
    let call_id = inner.next_tool_call_id();
    let iteration = inner.current_iteration;
    Some((call_id, iteration))
}

fn process_recorder() -> Option<Recorder> {
    let cell = PROCESS_RECORDER.get()?;
    cell.lock().ok()?.clone()
}

fn append_file(path: &Path) -> Result<File, io::Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn write_bytes_at(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    // All recorder writes flow through `write_atomic`, which
    // establishes atomicity, content durability, and dirent
    // durability (see `ooda_core::atomic_io`). Invariant: stable
    // read-surface files are never observable in a torn state by
    // a concurrent reader and never survive a crash truncated.
    ooda_core::atomic_io::write_atomic(path, bytes)
}

fn pull_request_root(root: &Path, slug: &RepoSlug, pr: PullRequestNumber) -> PathBuf {
    root.join("github.com")
        .join(slug.owner().as_str())
        .join(slug.repo().as_str())
        .join("prs")
        .join(pr.to_string())
}

fn run_id(now: DateTime<Utc>) -> String {
    format!(
        "{}-{:09}-p{}",
        now.format("%Y%m%dT%H%M%SZ"),
        now.timestamp_subsec_nanos(),
        std::process::id()
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn decision_summary(decision: &Decision) -> String {
    match decision {
        Decision::Execute(action) => action_summary(action),
        Decision::Halt(halt) => match halt {
            crate::decide::decision::DecisionHalt::Success => "halt success".to_string(),
            crate::decide::decision::DecisionHalt::Terminal(t) => {
                format!("halt terminal {t:?}")
            }
            crate::decide::decision::DecisionHalt::AgentNeeded(handoff) => {
                format!("handoff agent {}", handoff_action_summary(handoff))
            }
            crate::decide::decision::DecisionHalt::HumanNeeded(handoff) => {
                format!("handoff human {}", handoff_action_summary(handoff))
            }
        },
    }
}

fn decision_projection(decision: &Decision) -> Value {
    match decision {
        Decision::Execute(action) => json!({
            "type": "execute",
            "action": action_projection(action),
        }),
        Decision::Halt(halt) => match halt {
            crate::decide::decision::DecisionHalt::Success => {
                json!({ "type": "halt", "halt": "success" })
            }
            crate::decide::decision::DecisionHalt::Terminal(t) => {
                json!({ "type": "halt", "halt": "terminal", "terminal": format!("{t:?}") })
            }
            crate::decide::decision::DecisionHalt::AgentNeeded(handoff) => json!({
                "type": "halt",
                "halt": "agent_needed",
                "action": handoff_action_projection(handoff),
            }),
            crate::decide::decision::DecisionHalt::HumanNeeded(handoff) => json!({
                "type": "halt",
                "halt": "human_needed",
                "action": handoff_action_projection(handoff),
            }),
        },
    }
}

fn action_summary(action: &Action) -> String {
    format!(
        "{} ({:?}) blocker: {}",
        action.kind.name(),
        action.effect,
        action.blocker
    )
}

fn action_projection(action: &Action) -> Value {
    json!({
        "kind": action.kind.name(),
        "target_effect": format!("{:?}", action.target_effect),
        "urgency": format!("{:?}", action.urgency),
        "blocker": action.blocker.to_string(),
        "effect": &action.effect,
    })
}

fn handoff_action_summary(
    handoff: &ooda_core::HandoffAction<crate::decide::action::ActionKind>,
) -> String {
    format!("{} blocker: {}", handoff.kind.name(), handoff.blocker)
}

fn handoff_action_projection(
    handoff: &ooda_core::HandoffAction<crate::decide::action::ActionKind>,
) -> Value {
    json!({
        "kind": handoff.kind.name(),
        "target_effect": format!("{:?}", handoff.target_effect),
        "urgency": format!("{:?}", handoff.urgency),
        "blocker": handoff.blocker.to_string(),
        "prompt": &handoff.prompt,
    })
}

fn outcome_summary(outcome: &Outcome, code: ExitCode) -> String {
    format!("{outcome:?} exit={code}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::ids::BlockerKey;
    use ooda_core::MidTier;
    use ooda_core::{HandoffPrompt, PollingInterval};

    // ─── recorder JSONL schema goldens ─────────────────────────────
    //
    // Exhaustiveness over `ActionEffect` is structural: the match
    // in `recorder_action_golden` denies a non-exhaustive arm at
    // compile time. Adding a variant requires extending the golden
    // — the type system catches schema drift before runtime.

    fn sample_action(effect: ActionEffect) -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    /// Canonical schema for an `action_projection` output: a
    /// constant outer object whose `effect` field carries the
    /// variant-specific tail. The outer shape is invariant; the
    /// effect tail is variant-dispatched.
    fn recorder_action_golden(action: &Action) -> Value {
        let effect_json = match &action.effect {
            ActionEffect::Full { log } => json!({"Full": {"log": log}}),
            ActionEffect::Wait { interval, log } => json!({
                "Wait": {
                    "interval": {
                        "secs": interval.as_duration().as_secs(),
                        "nanos": interval.as_duration().subsec_nanos(),
                    },
                    "log": log,
                }
            }),
            ActionEffect::Agent { prompt } => json!({
                "Agent": {"prompt": prompt_golden(prompt)}
            }),
            ActionEffect::Human { prompt } => json!({
                "Human": {"prompt": prompt_golden(prompt)}
            }),
        };
        json!({
            "kind": action.kind.name(),
            "target_effect": format!("{:?}", action.target_effect),
            "urgency": format!("{:?}", action.urgency),
            "blocker": action.blocker.to_string(),
            "effect": effect_json,
        })
    }

    /// Mirror of `HandoffPrompt`'s `Serialize` shape (headline +
    /// sections). Independent re-derivation: the golden is correct
    /// iff it matches serde's output, so a `Serialize` change
    /// regresses here loudly.
    fn prompt_golden(prompt: &HandoffPrompt) -> Value {
        json!({
            "headline": prompt.headline.as_str(),
            "sections": prompt.sections,
        })
    }

    /// Sample coverage over `ActionEffect`: one inhabitant per
    /// variant. Hand-maintained; the length-sentinel assertion
    /// guards against silent drift if a new variant is added but
    /// no sample is.
    fn recorder_sample_effects() -> Vec<ActionEffect> {
        vec![
            ActionEffect::Full {
                log: "Mark PR ready".into(),
            },
            ActionEffect::Wait {
                interval: PollingInterval::from_secs(30),
                log: "Waiting for CI".into(),
            },
            ActionEffect::Agent {
                prompt: HandoffPrompt::new("Address review threads"),
            },
            ActionEffect::Human {
                prompt: HandoffPrompt::new("Request or self-approve"),
            },
        ]
    }

    /// Variant-wise golden assertions for `action_projection`'s
    /// schema. Compile-time exhaustiveness over `ActionEffect` is
    /// supplied by `recorder_action_golden`'s match plus serde's
    /// derived `Serialize`; runtime exhaustiveness over the sample
    /// list is supplied by the length-sentinel below.
    #[test]
    fn recorder_action_projection_schema_goldens() {
        let samples = recorder_sample_effects();
        assert_eq!(
            samples.len(),
            4,
            "`recorder_sample_effects` must include one sample per `ActionEffect` variant; \
             adding a new variant requires adding both a golden arm in `recorder_action_golden` \
             AND a sample here.",
        );
        for effect in samples {
            let action = sample_action(effect);
            let actual = action_projection(&action);
            let expected = recorder_action_golden(&action);
            assert_eq!(
                actual, expected,
                "schema mismatch for ActionEffect: {:?}",
                action.effect
            );
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ooda-pr-recorder-test-{label}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn pull_request_root_is_repo_scoped() {
        let slug = RepoSlug::parse("acme/widgets").unwrap();
        let pr = PullRequestNumber::new(42).unwrap();
        let root = pull_request_root(Path::new("/state"), &slug, pr);
        assert_eq!(root, PathBuf::from("/state/github.com/acme/widgets/prs/42"));
    }

    #[test]
    fn sha256_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn outcome_artifact_is_linked_to_compressed_blob_and_published_in_current() {
        let root = temp_root("blob");
        let _ = fs::remove_dir_all(&root);
        let recorder = Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("example/widgets").unwrap(),
            pr: PullRequestNumber::new(7).unwrap(),
            mode: RunMode::Inspect,
            max_iter: std::num::NonZeroU32::new(1).expect("1 is non-zero"),
            status_comment: false,
            state_root: Some(root.clone()),
            legacy_trace: None,
        })
        .unwrap();

        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        let pr_root = recorder.pull_request_root();
        // Two layouts coexist: the pointer manifest names the
        // outcome.json path; the same bytes also live
        // content-addressed in the dedup store. Reachability via
        // either path is invariant.
        let current_bytes = fs::read(pr_root.join("CURRENT.json")).unwrap();
        let current: ooda_core::CurrentManifest = serde_json::from_slice(&current_bytes).unwrap();
        assert_eq!(current.headline, "Paused");
        assert_eq!(current.exit_code, ExitCode::Paused.as_u8());

        let outcome_path = pr_root.join(current.artifacts.get("outcome").unwrap());
        let outcome = fs::read(&outcome_path).unwrap();
        let hash = sha256_hex(&outcome);
        let blob = pr_root
            .join("blobs")
            .join("sha256")
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(format!("{hash}.zst"));
        let decoded = zstd::stream::decode_all(&*fs::read(blob).unwrap()).unwrap();
        assert_eq!(decoded, outcome);

        let events = fs::read_to_string(pr_root.join("events.jsonl")).unwrap();
        assert!(events.contains(r#""kind":"outcome""#), "{events}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_handoff_md_persists_body_at_per_iteration_path() {
        let root = temp_root("handoff_md");
        let _ = fs::remove_dir_all(&root);
        let recorder = Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("example/widgets").unwrap(),
            pr: PullRequestNumber::new(7).unwrap(),
            mode: RunMode::Inspect,
            max_iter: std::num::NonZeroU32::new(1).expect("1 is non-zero"),
            status_comment: false,
            state_root: Some(root.clone()),
            legacy_trace: None,
        })
        .unwrap();
        // Precondition for `write_handoff_md`: a current iteration
        // is set. The runner establishes this via `set_iteration`
        // before each iteration; tests must re-establish it.
        recorder.set_iteration(Some(1));

        let body = "Rebase onto base\n\nContinuation line.";
        let path = recorder
            .write_handoff_md(body)
            .expect("write should succeed under temp root");

        assert!(
            path.to_string_lossy().contains("/runs/")
                && path
                    .to_string_lossy()
                    .ends_with("/iterations/0001/handoff.md"),
            "handoff.md lives per-iteration, got {path:?}",
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
        let _ = fs::remove_dir_all(root);
    }

    // ── iteration_decided envelope golden ──
    //
    // The on-disk envelope is a load-bearing contract: downstream
    // readers index artifacts positionally. Lock the artifact count
    // and order with a golden so silent reordering fails CI.

    fn ts(s: &str) -> crate::ids::Timestamp {
        crate::ids::Timestamp::parse(s).unwrap()
    }

    fn empty_oriented_for_golden() -> OrientedState {
        use crate::observe::github::pull_request_view::Mergeable;
        use crate::orient::ci::{CheckBucket, CiActivity, CiReport, CiSummary, ResolvedState};
        use crate::orient::reviews::{PendingReviews, ReviewSummary};
        use crate::orient::state::PullRequestProjection;
        OrientedState {
            ci: CiReport {
                summary: CiSummary {
                    required: CheckBucket::default(),
                    missing_names: vec![],
                    completed_at: None,
                    advisory: CheckBucket::default(),
                },
                activity: CiActivity::Resolved(ResolvedState::AllGreen),
            },
            state: PullRequestProjection {
                conflict: Mergeable::Mergeable,
                draft: false,
                wip: false,
                title_len: 30,
                title_ok: true,
                body: true,
                summary: true,
                test_plan: true,
                content_label: true,
                assignees: 1,
                reviewers: 1,
                merge_when_ready: false,
                commits: 1,
                behind: false,
                has_open_parent_pr: false,
                merge_state_status:
                    crate::observe::github::pull_request_view::MergeStateStatus::Clean,
                updated_at: ts("2026-04-23T10:00:00Z"),
                last_commit_at: None,
                active_branch_rule_types: vec![],
                required_check_names_per_ruleset: vec![],
                missing_required_check_names_on_head: vec![],
            },
            reviews: ReviewSummary {
                decision: None,
                threads_unresolved: 0,
                threads_total: 0,
                bot_comments: 0,
                approvals_on_head: 0,
                approvals_stale: 0,
                pending_reviews: PendingReviews::default(),
                bot_reviews: vec![],
                requested_reviewers: crate::orient::reviews::RequestedReviewerSet::default(),
                latest_human_changes_requested: None,
            },
            copilot: None,
            cursor: None,
            threads: vec![],
            merge_base_delta: None,
            pull_request_metadata:
                crate::orient::pull_request_metadata::PullRequestMetadata::Synced,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::Synced,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            closeout: crate::orient::closeout::Closeout::Synced,
            closeout_attest_path: None,
        }
    }

    #[test]
    fn iteration_decided_event_carries_five_artifact_refs() {
        let root = temp_root("iter-decided");
        let _ = fs::remove_dir_all(&root);
        let recorder = Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("example/widgets").unwrap(),
            pr: PullRequestNumber::new(7).unwrap(),
            mode: RunMode::Loop,
            max_iter: std::num::NonZeroU32::new(1).expect("1 is non-zero"),
            status_comment: false,
            state_root: Some(root.clone()),
            legacy_trace: None,
        })
        .unwrap();

        let oriented = empty_oriented_for_golden();
        // Invariant: the artifact envelope is emitted in full on
        // every iteration regardless of `Decision` disposition.
        // `Halt::Success` exercises the empty-candidate path
        // without weakening that contract.
        let decision = Decision::Halt(crate::decide::decision::DecisionHalt::Success);

        recorder.record_iteration(1, &serde_json::json!({}), &oriented, &[], &decision);

        let pr_root = recorder.pull_request_root();
        let events = fs::read_to_string(pr_root.join("events.jsonl")).unwrap();
        let line = events
            .lines()
            .find(|l| l.contains(r#""kind":"iteration_decided""#))
            .expect("iteration_decided event present");
        let event: Value = serde_json::from_str(line).unwrap();

        let artifacts = event.get("artifacts").expect("artifacts field");
        let arr = artifacts.as_array().expect("artifacts is an array");
        assert_eq!(
            arr.len(),
            5,
            "envelope cardinality is load-bearing: 5 artifact refs \
             in fixed order (observations, oriented, candidates, \
             decision, dashboard). Extending requires coordinated \
             updates to downstream consumers AND this golden.",
        );
        let paths: Vec<String> = arr
            .iter()
            .map(|a| {
                a.get("path")
                    .and_then(Value::as_str)
                    .expect("artifact has path")
                    .to_string()
            })
            .collect();
        // Basename-only assertion: the full per-iteration path
        // shape is exercised by a sibling test. Separation of
        // concerns: this test pins envelope schema; the other
        // pins layout.
        let basenames: Vec<&str> = paths
            .iter()
            .map(|p| p.rsplit('/').next().expect("non-empty path"))
            .collect();
        assert_eq!(
            basenames,
            vec![
                "normalized.json",
                "oriented.json",
                "candidates.json",
                "decision_envelope.json",
                "dashboard.json",
            ],
        );
        // Two projections, two filenames: `dashboard.json` is the
        // tier-grouped human-shaped projection; `decision_envelope.json`
        // is the runner's typed dispatch input. The pointer manifest
        // routes consumers to one schema per filename.
        assert!(
            basenames.contains(&"dashboard.json"),
            "dashboard.json membership is a load-bearing contract for downstream tier-grouped consumers",
        );

        // Envelope-level fields the readers depend on.
        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("iteration_decided")
        );
        assert!(event.get("data").is_some(), "data envelope present");
        let data = &event["data"];
        assert_eq!(data.get("candidate_count").and_then(Value::as_u64), Some(0));
        assert!(
            data.get("decision").is_some(),
            "decision projection present"
        );

        let _ = fs::remove_dir_all(root);
    }
}
