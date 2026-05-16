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

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::Decision;
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::orient::OrientedState;
use crate::outcome::Outcome;
use ooda_core::ExitCode;

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
    pub max_iter: std::num::NonZeroU32,
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
    pub fn open(cfg: RecorderConfig) -> Result<Self, RecorderError> {
        let state_root =
            ooda_core::state_root::resolve_ooda_pr_state_root(cfg.state_root.as_deref());
        let pr_root = pull_request_root(&state_root, &cfg.slug, cfg.pr);
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

    pub fn pull_request_root(&self) -> PathBuf {
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

    pub fn record_iteration<TObs>(
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
            let decision_ref = inner.write_json_artifact(
                Some(iteration),
                "decision.json",
                decision,
                "application/json",
            )?;
            // Per spec: `latest/decision.json` carries the new
            // tier-grouped dashboard projection, not the executor
            // envelope. Keep per-iter `decision.json` (the
            // Execute/Halt envelope) for internal cross-referencing;
            // surface the human-shaped Dashboard at `latest/`.
            let dashboard_ref = inner.write_json_artifact(
                Some(iteration),
                "dashboard.json",
                &dashboard,
                "application/json",
            )?;

            inner.copy_latest("state.json", &oriented_ref)?;
            inner.copy_latest("decision.json", &dashboard_ref)?;
            // Serialize whichever payload the decision carries
            // (Action for Execute / Stuck-equivalents; HandoffAction
            // for AgentNeeded/HumanNeeded). Both implement Serialize
            // with their own schema; downstream tooling discriminates
            // by the surrounding decision-projection record.
            let action_ref = match decision {
                Decision::Execute(action) => Some(inner.write_json_artifact(
                    Some(iteration),
                    "action.json",
                    action,
                    "application/json",
                )?),
                Decision::Halt(
                    crate::decide::decision::DecisionHalt::AgentNeeded(handoff)
                    | crate::decide::decision::DecisionHalt::HumanNeeded(handoff),
                ) => Some(inner.write_json_artifact(
                    Some(iteration),
                    "action.json",
                    handoff,
                    "application/json",
                )?),
                Decision::Halt(_) => None,
            };
            if let Some(action_ref) = &action_ref {
                inner.copy_latest("action.json", action_ref)?;
            } else {
                inner.remove_latest("action.json")?;
            }
            inner.write_latest_markdown(iteration, decision)?;
            inner.write_blockers_markdown(&dashboard)?;
            inner.write_next_markdown(&dashboard)?;

            let artifacts = vec![
                obs_ref,
                oriented_ref,
                candidates_ref,
                decision_ref,
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

    pub fn record_outcome(&self, outcome: &Outcome, code: ExitCode) {
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
        let first_sequence = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<EventRange>(&bytes)
                .map_or(sequence, |range| range.first_sequence),
            Err(_) => sequence,
        };
        let range = EventRange {
            first_sequence,
            last_sequence: sequence,
        };
        Self::write_json_at(&path, &range)
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

    fn write_blockers_markdown(&self, dashboard: &Dashboard) -> Result<(), RecorderError> {
        let body = dashboard.render_blockers_md();
        write_bytes_at(&self.pr_root.join("latest/blockers.md"), body.as_bytes())?;
        Ok(())
    }

    fn write_next_markdown(&self, dashboard: &Dashboard) -> Result<(), RecorderError> {
        let body = dashboard.render_next_md();
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
        "effect": &action.effect,
        "target_effect": format!("{:?}", action.target_effect),
        "urgency": format!("{:?}", action.urgency),
        "blocker": action.blocker.to_string(),
        "description": action.rendered_payload(),
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
        "prompt": &handoff.prompt,
        "target_effect": format!("{:?}", handoff.target_effect),
        "urgency": format!("{:?}", handoff.urgency),
        "blocker": handoff.blocker.to_string(),
        "description": handoff.prompt.to_string(),
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
    use ooda_core::{HandoffPrompt, PollingInterval};

    // ─── recorder JSONL schema goldens ─────────────────────────────
    //
    // Exhaustive snapshot tests for `action_projection`. The contract
    // is the field set written into per-iteration JSONL records under
    // the recorder tree. The match in `recorder_action_golden` is
    // exhaustive over `ActionEffect`, so adding a new variant fails
    // to compile until a golden arm is added.

    fn sample_action(effect: ActionEffect) -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag("rebase-needed"),
        }
    }

    /// Canonical JSON shape for an `Action` written into the
    /// recorder JSONL by `action_projection`. The variant-specific
    /// tail lives inside `effect`; the surrounding object fields
    /// are constant across variants.
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
            "effect": effect_json,
            "target_effect": format!("{:?}", action.target_effect),
            "urgency": format!("{:?}", action.urgency),
            "blocker": action.blocker.to_string(),
            "description": action.rendered_payload(),
        })
    }

    fn prompt_golden(prompt: &HandoffPrompt) -> Value {
        json!({
            "headline": prompt.headline.as_str(),
            "sections": prompt.sections,
        })
    }

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
    fn outcome_artifact_is_linked_to_compressed_blob() {
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

        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused);

        let pr_root = recorder.pull_request_root();
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

    // ── iteration_decided envelope golden ──
    //
    // Phase A (b272b80) added `dashboard_ref` as the 5th artifact in
    // the on-disk `iteration_decided` event. The envelope contract is
    // load-bearing for downstream readers — lock it with a golden so
    // a future drop or reorder fails CI rather than silently breaking
    // the caller surface.

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
            codex_review: None,
            threads: vec![],
            merge_base_delta: None,
            pull_request_metadata:
                crate::orient::pull_request_metadata::PullRequestMetadata::Synced,
            attest_path: None,
            doc_review: crate::orient::doc_review::DocReview::Synced,
            doc_review_attest_path: None,
            claude_review: crate::orient::claude_review::ClaudeReview::NoActivity,
            claude_review_attest_path: None,
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
            "iteration_decided envelope must carry 5 artifact refs \
             (observations, oriented, candidates, decision, dashboard); \
             adding a 6th artifact requires updating this golden AND \
             downstream consumers.",
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
                "decision.json",
                "dashboard.json",
            ],
        );
        assert!(
            basenames.contains(&"dashboard.json"),
            "dashboard_ref must appear in artifacts (Phase A contract)",
        );

        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("iteration_decided"),
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
