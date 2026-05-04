//! Always-on PR memory harness.
//!
//! The recorder is the single persistence boundary for `ooda-pr`.
//! Runtime code reports observations, decisions, tool calls, actions,
//! comments, waits, and outcomes here; this module owns the on-disk
//! layout, event ordering, artifact storage, and latest/ledger views.

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

use crate::decide::action::Action;
use crate::decide::decision::Decision;
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::outcome::Outcome;

const SCHEMA_VERSION: u32 = 1;

static PROCESS_RECORDER: OnceLock<Mutex<Option<Recorder>>> = OnceLock::new();

#[derive(Debug)]
pub enum RecorderError {
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
pub enum RunMode {
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
pub struct RecorderConfig {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub mode: RunMode,
    pub max_iter: u32,
    pub status_comment: bool,
    pub state_root: Option<PathBuf>,
    pub legacy_trace: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactRef {
    pub path: String,
    pub blob: String,
    pub sha256: String,
    pub bytes: usize,
    pub media_type: String,
}

#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    slug: RepoSlug,
    pr: PullRequestNumber,
    mode: RunMode,
    max_iter: u32,
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
    max_iter: u32,
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
    pub fn open(cfg: RecorderConfig) -> Result<Self, RecorderError> {
        let state_root = resolve_state_root(cfg.state_root.as_deref());
        let pr_root = pr_root(&state_root, &cfg.slug, cfg.pr);
        let now = Utc::now();
        let run_id = run_id(now);
        let run_root = pr_root.join("runs").join(&run_id);

        fs::create_dir_all(run_root.join("iterations"))?;
        fs::create_dir_all(pr_root.join("latest"))?;
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

    pub fn install_process_recorder(&self) {
        let cell = PROCESS_RECORDER.get_or_init(|| Mutex::new(None));
        if let Ok(mut slot) = cell.lock() {
            *slot = Some(self.clone());
        }
    }

    pub fn set_iteration(&self, iteration: Option<u32>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.current_iteration = iteration;
        }
    }

    pub fn pr_root(&self) -> PathBuf {
        self.with_inner(|inner| inner.pr_root.clone())
            .unwrap_or_default()
    }

    pub fn dedup_path(&self) -> PathBuf {
        self.with_inner(|inner| inner.pr_root.join("status-comment").join("dedup.json"))
            .unwrap_or_default()
    }

    pub fn write_trace_line(&self, line: &str) {
        self.best_effort(|inner| {
            writeln!(inner.trace_md, "{line}")?;
            if let Some(file) = inner.legacy_trace.as_mut() {
                writeln!(file, "{line}")?;
            }
            Ok(())
        });
    }

    pub fn record_iteration<TObs, TOriented>(
        &self,
        iteration: u32,
        observations: &TObs,
        oriented: &TOriented,
        candidates: &[Action],
        decision: &Decision,
    ) where
        TObs: Serialize,
        TOriented: Serialize,
    {
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
            let decision_ref = inner.write_json_artifact(
                Some(iteration),
                "decision.json",
                decision,
                "application/json",
            )?;

            inner.copy_latest("state.json", &oriented_ref)?;
            inner.copy_latest("decision.json", &decision_ref)?;
            if let Some(action) = decision_action(decision) {
                let action_ref = inner.write_json_artifact(
                    Some(iteration),
                    "action.json",
                    action,
                    "application/json",
                )?;
                inner.copy_latest("action.json", &action_ref)?;
            } else {
                inner.remove_latest("action.json")?;
            }
            inner.write_latest_markdown(iteration, decision)?;
            inner.write_blockers_markdown(decision)?;
            inner.write_next_markdown(decision)?;

            let artifacts = vec![obs_ref, oriented_ref, candidates_ref, decision_ref];
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

    pub fn record_observe_start(&self, iteration: u32) {
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

    pub fn record_observe_end(&self, iteration: u32, result: Result<(), String>) {
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

    pub fn record_status_comment_rendered<T: Serialize>(
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

    pub fn record_status_comment_result<T: Serialize>(
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

    pub fn record_action_start(&self, iteration: u32, action: &Action) {
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

    pub fn record_action_end(&self, iteration: u32, action: &Action, result: Result<(), String>) {
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

    pub fn record_wait_start(&self, iteration: u32, action: &Action) {
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

    pub fn record_wait_end(&self, iteration: u32, action: &Action) {
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

    pub fn record_outcome(&self, outcome: &Outcome, code: u8) {
        self.best_effort(|inner| {
            let artifact =
                inner.write_json_artifact(None, "outcome.json", outcome, "application/json")?;
            inner.copy_latest("outcome.json", &artifact)?;
            inner.append_event(
                inner.current_iteration,
                "outcome",
                outcome_summary(outcome, code),
                vec![artifact],
                json!({ "exit_code": code }),
            )?;
            inner.append_ledger("outcome", &outcome_summary(outcome, code))?;
            writeln!(inner.trace_md, "exit={code}")?;
            if let Some(file) = inner.legacy_trace.as_mut() {
                writeln!(file, "exit={code}")?;
            }
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
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unknown>".to_string()),
            argv: std::env::args().collect(),
        };
        inner.write_json_at(&inner.run_root.join("manifest.json"), &manifest)?;
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
        let first_sequence = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<EventRange>(&bytes)
                .map(|range| range.first_sequence)
                .unwrap_or(sequence),
            Err(_) => sequence,
        };
        let range = EventRange {
            first_sequence,
            last_sequence: sequence,
        };
        self.write_json_at(&path, &range)
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

    fn write_json_at<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), RecorderError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        write_bytes_at(path, &bytes)?;
        Ok(())
    }

    fn copy_latest(&self, name: &str, artifact: &ArtifactRef) -> Result<(), RecorderError> {
        let source = self.pr_root.join(&artifact.path);
        let target = self.pr_root.join("latest").join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
        Ok(())
    }

    fn remove_latest(&self, name: &str) -> Result<(), RecorderError> {
        match fs::remove_file(self.pr_root.join("latest").join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn write_latest_markdown(
        &self,
        iteration: u32,
        decision: &Decision,
    ) -> Result<(), RecorderError> {
        let body = format!(
            "# ooda-pr latest\n\n- repo: `{}`\n- pr: `{}`\n- run: `{}`\n- iteration: `{}`\n- decision: `{}`\n\nLinks:\n- [state](state.json)\n- [decision](decision.json)\n- [action](action.json)\n- [outcome](outcome.json)\n- [ledger](../ledger.md)\n- [events](../events.jsonl)\n",
            self.slug,
            self.pr,
            self.run_id,
            iteration,
            decision_summary(decision),
        );
        write_bytes_at(&self.pr_root.join("latest/index.md"), body.as_bytes())?;
        Ok(())
    }

    fn write_blockers_markdown(&self, decision: &Decision) -> Result<(), RecorderError> {
        let body = match decision_action(decision) {
            Some(action) => format!(
                "# Blockers\n\n- `{}` via `{}`\n",
                action.blocker,
                action.kind.name()
            ),
            None => "# Blockers\n\nNo current blocker.\n".to_string(),
        };
        write_bytes_at(&self.pr_root.join("latest/blockers.md"), body.as_bytes())?;
        Ok(())
    }

    fn write_next_markdown(&self, decision: &Decision) -> Result<(), RecorderError> {
        let body = match decision_action(decision) {
            Some(action) => format!(
                "# Next\n\n{}\n\n- action: `{}`\n- automation: `{:?}`\n- blocker: `{}`\n",
                action.description,
                action.kind.name(),
                action.automation,
                action.blocker,
            ),
            None => "# Next\n\nNo action selected.\n".to_string(),
        };
        write_bytes_at(&self.pr_root.join("latest/next.md"), body.as_bytes())?;
        Ok(())
    }
}

pub struct ToolCallGuard {
    recorder: Recorder,
    call_id: String,
    program: String,
    args: Vec<String>,
    cwd: String,
    started: Instant,
    iteration: Option<u32>,
}

pub fn tool_call_started(program: &str, args: &[&str]) -> Option<ToolCallGuard> {
    let recorder = process_recorder()?;
    let (call_id, iteration) = next_tool_call_id_locked(&recorder)?;
    let args_v: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());

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
    pub fn finish_output(self, output: &Output) {
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
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".to_string())
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

    pub fn finish_spawn_error(self, err: &io::Error) {
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
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

fn resolve_state_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = nonempty_env_path("OODA_PR_STATE_HOME") {
        return path;
    }
    if let Some(path) = nonempty_env_path("XDG_STATE_HOME") {
        return path.join("ooda-pr");
    }
    if let Some(home) = nonempty_env_path("HOME") {
        return home.join(".local").join("state").join("ooda-pr");
    }
    std::env::temp_dir().join("ooda-pr")
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn pr_root(root: &Path, slug: &RepoSlug, pr: PullRequestNumber) -> PathBuf {
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

fn decision_action(decision: &Decision) -> Option<&Action> {
    match decision {
        Decision::Execute(action) => Some(action),
        Decision::Halt(crate::decide::decision::DecisionHalt::AgentNeeded(action))
        | Decision::Halt(crate::decide::decision::DecisionHalt::HumanNeeded(action)) => {
            Some(action)
        }
        Decision::Halt(_) => None,
    }
}

fn decision_summary(decision: &Decision) -> String {
    match decision {
        Decision::Execute(action) => action_summary(action),
        Decision::Halt(halt) => match halt {
            crate::decide::decision::DecisionHalt::Success => "halt success".to_string(),
            crate::decide::decision::DecisionHalt::Terminal(t) => {
                format!("halt terminal {t:?}")
            }
            crate::decide::decision::DecisionHalt::AgentNeeded(action) => {
                format!("handoff agent {}", action_summary(action))
            }
            crate::decide::decision::DecisionHalt::HumanNeeded(action) => {
                format!("handoff human {}", action_summary(action))
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
            crate::decide::decision::DecisionHalt::AgentNeeded(action) => json!({
                "type": "halt",
                "halt": "agent_needed",
                "action": action_projection(action),
            }),
            crate::decide::decision::DecisionHalt::HumanNeeded(action) => json!({
                "type": "halt",
                "halt": "human_needed",
                "action": action_projection(action),
            }),
        },
    }
}

fn action_summary(action: &Action) -> String {
    format!(
        "{} ({:?}) blocker: {}",
        action.kind.name(),
        action.automation,
        action.blocker
    )
}

fn action_projection(action: &Action) -> Value {
    json!({
        "kind": action.kind.name(),
        "automation": format!("{:?}", action.automation),
        "target_effect": format!("{:?}", action.target_effect),
        "urgency": format!("{:?}", action.urgency),
        "blocker": action.blocker.to_string(),
        "description": action.description,
    })
}

fn outcome_summary(outcome: &Outcome, code: u8) -> String {
    format!("{outcome:?} exit={code}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ooda-pr-recorder-test-{label}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn pr_root_is_repo_pr_scoped() {
        let slug = RepoSlug::parse("acme/widgets").unwrap();
        let pr = PullRequestNumber::new(42).unwrap();
        let root = pr_root(Path::new("/state"), &slug, pr);
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
    fn outcome_artifact_is_linked_to_compressed_blob() {
        let root = temp_root("blob");
        let _ = fs::remove_dir_all(&root);
        let recorder = Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("example/widgets").unwrap(),
            pr: PullRequestNumber::new(7).unwrap(),
            mode: RunMode::Inspect,
            max_iter: 1,
            status_comment: false,
            state_root: Some(root.clone()),
            legacy_trace: None,
        })
        .unwrap();

        recorder.record_outcome(&Outcome::Paused, 7);

        let pr_root = recorder.pr_root();
        let outcome = fs::read(pr_root.join("latest/outcome.json")).unwrap();
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
}
