//! Per-PR persistence — thin adapter over [`ooda_state`].
//!
//! # Role
//!
//! Sole on-disk boundary for the PR loop. Runtime code reports
//! observations / decisions / handoffs / tool calls through this
//! module; the domain-neutral [`ooda_state`] crate owns the layout
//! (events.jsonl + content-addressed blobs).
//!
//! # Invariants
//!
//! - **Single writer per run**: one [`Recorder`] per invocation;
//!   internal mutation serialised by `Arc<Mutex<_>>`.
//! - **Append-only event log**: every emission goes through
//!   [`ooda_state::RunWriter::append`] (or `start` / `halt`).
//! - **Content-addressed payloads**: artifact bytes round-trip via
//!   [`ooda_state::RunWriter::write_blob`]; the resulting `BlobRef`
//!   is the only handle embedded in events.
//! - **Process-singleton for tool calls**: a `OnceLock<Mutex<Option>>`
//!   holds the active recorder so subprocess wrappers
//!   (`observe::github::gh`) can record without threading the
//!   recorder through every call site.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use ooda_state::{BlobRef, EventBody, RunId, RunWriter, StateError, StateRoot, resolve_state_root};

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::compare::MergeBaseDelta;
use crate::orient::OrientedState;
use crate::orient::ci::CiReport;
use crate::orient::claude_review::ClaudeReview;
use crate::orient::closeout::Closeout;
use crate::orient::copilot::CopilotReport;
use crate::orient::cursor::CursorReport;
use crate::orient::doc_review::DocReview;
use crate::orient::pull_request_metadata::PullRequestMetadata;
use crate::orient::reviews::ReviewSummary;
use crate::orient::state::PullRequestProjection;
use crate::orient::thread::ReviewThread;
use crate::outcome::Outcome;
use ooda_core::ExitCode;

static PROCESS_RECORDER: OnceLock<Mutex<Option<Recorder>>> = OnceLock::new();

#[derive(Debug)]
pub(crate) enum RecorderError {
    State(StateError),
    Io(io::Error),
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RecorderError {}

impl From<StateError> for RecorderError {
    fn from(e: StateError) -> Self {
        Self::State(e)
    }
}

impl From<io::Error> for RecorderError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Per-consumer input slice for [`Recorder::record_iteration`].
/// Each field declares a typed dep ref. The struct is the function
/// signature reified; its scope is exactly what this one consumer
/// reads (dashboard inputs + the `oriented` snapshot serialization).
///
/// Field order mirrors `OrientedState` so the derived `Serialize`
/// impl produces byte-identical JSON for the oriented blob.
#[derive(Serialize)]
pub(crate) struct RecorderInputs<'a> {
    pub ci: &'a CiReport,
    pub state: &'a PullRequestProjection,
    pub reviews: &'a ReviewSummary,
    pub copilot: Option<&'a CopilotReport>,
    pub cursor: Option<&'a CursorReport>,
    pub threads: &'a [ReviewThread],
    pub merge_base_delta: Option<&'a MergeBaseDelta>,
    pub pull_request_metadata: &'a PullRequestMetadata,
    pub attest_path: Option<&'a Path>,
    pub doc_review: &'a DocReview,
    pub doc_review_attest_path: Option<&'a Path>,
    pub claude_review: &'a ClaudeReview,
    pub claude_review_attest_path: Option<&'a Path>,
    pub closeout: &'a Closeout,
    pub closeout_attest_path: Option<&'a Path>,
}

impl<'a> From<&'a OrientedState> for RecorderInputs<'a> {
    fn from(o: &'a OrientedState) -> Self {
        Self {
            ci: &o.ci,
            state: &o.state,
            reviews: &o.reviews,
            copilot: o.copilot.as_ref(),
            cursor: o.cursor.as_ref(),
            threads: &o.threads,
            merge_base_delta: o.merge_base_delta.as_ref(),
            pull_request_metadata: &o.pull_request_metadata,
            attest_path: o.attest_path.as_deref(),
            doc_review: &o.doc_review,
            doc_review_attest_path: o.doc_review_attest_path.as_deref(),
            claude_review: &o.claude_review,
            claude_review_attest_path: o.claude_review_attest_path.as_deref(),
            closeout: &o.closeout,
            closeout_attest_path: o.closeout_attest_path.as_deref(),
        }
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

#[derive(Clone)]
pub(crate) struct Recorder {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    slug: RepoSlug,
    pr: PullRequestNumber,
    root: StateRoot,
    writer: RunWriter,
    current_iteration: Option<u32>,
    tool_sequence: u64,
    legacy_trace: Option<File>,
}

impl Recorder {
    pub(crate) fn open(cfg: RecorderConfig) -> Result<Self, RecorderError> {
        let root = StateRoot::new(resolve_state_root(cfg.state_root.as_deref()))?;
        let id = RunId::generate();
        let mut writer = root.create_run(id)?;

        let legacy_trace = match cfg.legacy_trace.as_deref() {
            Some(path) => Some(open_append(path)?),
            None => None,
        };

        let target = json!({
            "forge": "github.com",
            "slug": cfg.slug.to_string(),
            "pr": cfg.pr,
            "mode": cfg.mode,
            "max_iter": cfg.max_iter,
            "status_comment": cfg.status_comment,
            "cwd": std::env::current_dir()
                .map_or_else(|_| "<unknown>".to_string(), |p| p.display().to_string()),
            "argv": std::env::args().collect::<Vec<_>>(),
        });
        writer.start(EventBody::RunStarted {
            domain: "pr".into(),
            target,
        })?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                slug: cfg.slug,
                pr: cfg.pr,
                root,
                writer,
                current_iteration: None,
                tool_sequence: 0,
                legacy_trace,
            })),
        })
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

    /// Per-PR dedup file for status comments. Lives outside the
    /// per-run tree because dedup is a cross-run invariant: a fresh
    /// run must observe prior runs' posted hashes.
    pub(crate) fn dedup_path(&self) -> PathBuf {
        self.with_inner(|inner| pr_index_path(inner.root.path(), &inner.slug, inner.pr))
            .unwrap_or_default()
            .join("status-comment-dedup.json")
    }

    /// Persist a handoff prompt body as a content-addressed blob and
    /// return its absolute path. The stderr `see:` pointer targets
    /// this file verbatim.
    pub(crate) fn write_handoff_md(&self, prompt: &str) -> Option<PathBuf> {
        let mut inner = self.inner.lock().ok()?;
        let blob = inner.writer.write_blob(prompt.as_bytes(), "md").ok()?;
        let path = inner
            .root
            .path()
            .join("runs")
            .join(inner.writer.run_id().as_str())
            .join("blobs")
            .join(format!("{}.{}", blob.sha, blob.ext));
        let iteration = inner.current_iteration?;
        // Best-effort: announce the handoff via an event referencing
        // the blob. The path is also derivable from (run_id, sha) so
        // even a failed append leaves the file reachable.
        let _ = inner.writer.append(EventBody::IterationHandoff {
            iteration,
            variant: "Pending".into(),
            action_kind: "Pending".into(),
            blob,
        });
        Some(path)
    }

    pub(crate) fn write_trace_line(&self, line: &str) {
        self.best_effort(|inner| {
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
        inputs: &RecorderInputs<'_>,
        candidates: &[Action],
        decision: &Decision,
    ) where
        TObs: Serialize,
    {
        let dashboard = Dashboard::from_iteration(
            &crate::dashboard::DashboardInputs {
                ci: inputs.ci,
                cursor: inputs.cursor,
                copilot: inputs.copilot,
                pull_request_metadata: inputs.pull_request_metadata,
                doc_review: inputs.doc_review,
                claude_review: inputs.claude_review,
            },
            candidates,
            decision,
        );
        self.best_effort(|inner| {
            inner.current_iteration = Some(iteration);

            let obs_blob = inner.write_json_blob(observations)?;
            let oriented_blob = inner.write_json_blob(inputs)?;
            let _ = inner.write_json_blob(candidates)?;
            let _ = inner.write_json_blob(decision)?;
            let _ = inner.write_json_blob(&dashboard)?;

            inner.writer.append(EventBody::IterationObserved {
                iteration,
                blob: obs_blob,
            })?;
            inner.writer.append(EventBody::IterationOriented {
                iteration,
                blob: oriented_blob,
            })?;
            inner.writer.append(EventBody::IterationDecided {
                iteration,
                decision_kind: decision_kind(decision),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_observe_start(&self, iteration: u32) {
        self.best_effort(|inner| {
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "observe_started".into(),
                payload: json!({ "iteration": iteration }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_observe_end(&self, iteration: u32, result: Result<(), String>) {
        let success = result.is_ok();
        let error = result.err();
        self.best_effort(|inner| {
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "observe_finished".into(),
                payload: json!({
                    "iteration": iteration,
                    "success": success,
                    "error": error,
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_status_comment_rendered<T: Serialize>(
        &self,
        iteration: Option<u32>,
        rendered: &T,
        summary: impl Into<String>,
    ) {
        let summary = summary.into();
        self.best_effort(|inner| {
            let blob = inner.write_json_blob(rendered)?;
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "status_comment_rendered".into(),
                payload: json!({
                    "iteration": iteration,
                    "summary": summary,
                    "blob": blob,
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_status_comment_result<T: Serialize>(
        &self,
        iteration: Option<u32>,
        result: &T,
        summary: impl Into<String>,
    ) {
        let summary = summary.into();
        self.best_effort(|inner| {
            let blob = inner.write_json_blob(result)?;
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "status_comment_result".into(),
                payload: json!({
                    "iteration": iteration,
                    "summary": summary,
                    "blob": blob,
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_action_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "action_started".into(),
                payload: json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_action_end(
        &self,
        iteration: u32,
        action: &Action,
        result: Result<(), String>,
    ) {
        let success = result.is_ok();
        let error = result.err();
        self.best_effort(|inner| {
            inner.writer.append(EventBody::IterationExecuted {
                iteration,
                action_kind: action.kind.name().to_string(),
            })?;
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "action_finished".into(),
                payload: json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                    "success": success,
                    "error": error,
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_wait_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "wait_started".into(),
                payload: json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_wait_end(&self, iteration: u32, action: &Action) {
        let interval_ms = match &action.effect {
            crate::decide::action::ActionEffect::Wait { interval, .. } => {
                u64::try_from(interval.as_duration().as_millis()).unwrap_or(u64::MAX)
            }
            _ => 0,
        };
        self.best_effort(|inner| {
            inner.writer.append(EventBody::IterationWaited {
                iteration,
                action_kind: action.kind.name().to_string(),
                interval_ms,
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_outcome(
        &self,
        outcome: &Outcome,
        code: ExitCode,
        headline: &str,
        handoff_path: Option<&Path>,
    ) {
        let outcome_name = outcome_name(outcome).to_string();
        self.best_effort(|inner| {
            let outcome_blob = inner.write_json_blob(outcome)?;
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "outcome_artifact".into(),
                payload: json!({
                    "blob": outcome_blob,
                    "headline": headline,
                    "handoff_path": handoff_path.map(|p| p.display().to_string()),
                }),
            })?;
            if let Some(file) = inner.legacy_trace.as_mut() {
                writeln!(file, "exit={code}")?;
            }
            let exit_code_i32 = i32::from(u8::from(code));
            // `halt` deletes the live marker. After this returns the
            // run no longer appears in `live/`; further appends are
            // best-effort.
            inner.writer.halt(EventBody::RunHalted {
                outcome: outcome_name,
                exit_code: exit_code_i32,
            })?;
            Ok(())
        });
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
    fn write_json_blob<T: Serialize + ?Sized>(&self, value: &T) -> Result<BlobRef, RecorderError> {
        let bytes = serde_json::to_vec_pretty(value).map_err(StateError::from)?;
        Ok(self.writer.write_blob(&bytes, "json")?)
    }

    fn next_tool_call_id(&mut self) -> String {
        self.tool_sequence += 1;
        format!("tc-{:06}", self.tool_sequence)
    }
}

// ── Tool-call hook for `observe::github::gh` ─────────────────────────

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
        inner.writer.append(EventBody::DomainSpecific {
            kind_suffix: "tool_call_started".into(),
            payload: json!({
                "iteration": iteration,
                "call_id": call_id,
                "program": program,
                "args": args_v,
                "cwd": cwd,
            }),
        })?;
        Ok(())
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
        let status_code = output.status.code();
        let success = output.status.success();
        self.recorder.best_effort(|inner| {
            let stdout_blob = inner.writer.write_blob(&output.stdout, "bin")?;
            let stderr_blob = inner.writer.write_blob(&output.stderr, "bin")?;
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "tool_call_finished".into(),
                payload: json!({
                    "iteration": self.iteration,
                    "call_id": self.call_id,
                    "program": self.program,
                    "args": self.args,
                    "cwd": self.cwd,
                    "duration_ms": duration_ms,
                    "status_code": status_code,
                    "success": success,
                    "stdout_blob": stdout_blob,
                    "stderr_blob": stderr_blob,
                }),
            })?;
            Ok(())
        });
    }

    pub(crate) fn finish_spawn_error(self, err: &io::Error) {
        let duration_ms = self.started.elapsed().as_millis();
        let err_str = err.to_string();
        self.recorder.best_effort(|inner| {
            inner.writer.append(EventBody::DomainSpecific {
                kind_suffix: "tool_call_finished".into(),
                payload: json!({
                    "iteration": self.iteration,
                    "call_id": self.call_id,
                    "program": self.program,
                    "args": self.args,
                    "cwd": self.cwd,
                    "duration_ms": duration_ms,
                    "success": false,
                    "error": err_str,
                }),
            })?;
            Ok(())
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

// ── Helpers ──────────────────────────────────────────────────────────

fn open_append(path: &Path) -> Result<File, io::Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Per-PR index directory: a stable per-`(slug, pr)` location for
/// cross-run state (e.g. status-comment dedup). Lives under
/// `<state-root>/index/pr/<owner>/<repo>/<pr>/`, parallel to the
/// run tree at `<state-root>/runs/`.
fn pr_index_path(root: &Path, slug: &RepoSlug, pr: PullRequestNumber) -> PathBuf {
    let dir = root
        .join("index")
        .join("pr")
        .join(slug.owner().as_str())
        .join(slug.repo().as_str())
        .join(pr.to_string());
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Single-token rendering for the `IterationDecided` event's
/// `decision_kind` field. Domain-stable; downstream readers
/// switch on this string.
fn decision_kind(decision: &Decision) -> String {
    match decision {
        Decision::Execute(action) => format!("Execute::{}", action.kind.name()),
        Decision::Halt(halt) => match halt {
            DecisionHalt::Success => "Halt::Success".into(),
            DecisionHalt::Terminal(t) => format!("Halt::Terminal({t:?})"),
            DecisionHalt::AgentNeeded(_) => "Halt::AgentNeeded".into(),
            DecisionHalt::HumanNeeded(_) => "Halt::HumanNeeded".into(),
        },
    }
}

fn action_projection(action: &Action) -> serde_json::Value {
    json!({
        "kind": action.kind.name(),
        "target_effect": format!("{:?}", action.target_effect),
        "urgency": format!("{:?}", action.urgency),
        "blocker": action.blocker.to_string(),
        "effect": &action.effect,
    })
}

fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::DoneSucceeded => "DoneSucceeded",
        Outcome::Paused => "Paused",
        Outcome::WouldAdvance(_) => "WouldAdvance",
        Outcome::HandoffHuman(_) => "HandoffHuman",
        Outcome::HandoffAgent(_) => "HandoffAgent",
        Outcome::DoneAborted => "DoneAborted",
        Outcome::StuckRepeated(_) => "StuckRepeated",
        Outcome::StuckCapReached(_) => "StuckCapReached",
        Outcome::UsageError(_) => "UsageError",
        Outcome::BinaryError(_) => "BinaryError",
    }
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

    fn recorder_action_golden(action: &Action) -> serde_json::Value {
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

    fn prompt_golden(prompt: &HandoffPrompt) -> serde_json::Value {
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
        std::env::temp_dir().join(format!("ooda-pr-state-test-{label}-{}", std::process::id()))
    }

    fn open_recorder(root: &Path) -> Recorder {
        let _ = fs::remove_dir_all(root);
        Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("example/widgets").unwrap(),
            pr: PullRequestNumber::new(7).unwrap(),
            mode: RunMode::Inspect,
            max_iter: std::num::NonZeroU32::new(1).expect("1 is non-zero"),
            status_comment: false,
            state_root: Some(root.to_path_buf()),
            legacy_trace: None,
        })
        .unwrap()
    }

    #[test]
    fn outcome_is_recorded_as_run_halted_with_blob() {
        let root = temp_root("outcome");
        let recorder = open_recorder(&root);

        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        // Walk the runs/ directory: exactly one run, with events.
        let runs = root.join("runs");
        let mut run_ids = Vec::new();
        for entry in fs::read_dir(&runs).unwrap() {
            run_ids.push(entry.unwrap().file_name().into_string().unwrap());
        }
        assert_eq!(run_ids.len(), 1, "exactly one run: {run_ids:?}");
        let events_path = runs.join(&run_ids[0]).join("events.jsonl");
        let body = fs::read_to_string(&events_path).unwrap();
        assert!(body.contains(r#""kind":"run_started""#), "{body}");
        assert!(body.contains(r#""kind":"run_halted""#), "{body}");
        assert!(body.contains(r#""outcome":"Paused""#), "{body}");

        // Live marker has been removed by `halt`.
        assert!(!root.join("live").join(&run_ids[0]).exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_handoff_md_persists_body_as_blob() {
        let root = temp_root("handoff");
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));

        let body = "Rebase onto base\n\nContinuation line.";
        let path = recorder
            .write_handoff_md(body)
            .expect("write should succeed under temp root");

        assert!(
            path.to_string_lossy().contains("/runs/") && path.to_string_lossy().contains("/blobs/"),
            "handoff blob lives under runs/<id>/blobs/, got {path:?}",
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dedup_path_is_per_pr_and_cross_run() {
        let root = temp_root("dedup");
        let a = open_recorder(&root).dedup_path();
        let b = open_recorder(&root).dedup_path();
        // Two distinct runs on the same (slug, pr) share the dedup
        // file: dedup is a cross-run invariant.
        assert_eq!(a, b);
        assert!(
            a.to_string_lossy().contains("index/pr/example/widgets/7"),
            "dedup path lives under index/pr/<slug>/<pr>/, got {a:?}",
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn iteration_decided_emits_three_events_with_blobs() {
        let root = temp_root("iter-decided");
        let recorder = open_recorder(&root);

        let oriented = empty_oriented_for_golden();
        let decision = Decision::Halt(DecisionHalt::Success);

        recorder.record_iteration(
            1,
            &serde_json::json!({}),
            &RecorderInputs::from(&oriented),
            &[],
            &decision,
        );

        let runs = root.join("runs");
        let entry = fs::read_dir(&runs).unwrap().next().unwrap().unwrap();
        let events_path = entry.path().join("events.jsonl");
        let body = fs::read_to_string(&events_path).unwrap();
        // Three iteration events emitted in order; each lifecycle
        // event refers to a content-addressed blob.
        assert!(body.contains(r#""kind":"iteration_observed""#), "{body}");
        assert!(body.contains(r#""kind":"iteration_oriented""#), "{body}");
        assert!(body.contains(r#""kind":"iteration_decided""#), "{body}");
        assert!(
            body.contains(r#""decision_kind":"Halt::Success""#),
            "{body}"
        );
        let _ = fs::remove_dir_all(root);
    }

    fn empty_oriented_for_golden() -> OrientedState {
        use crate::ids::Timestamp;
        use crate::observe::github::pull_request_view::{MergeStateStatus, Mergeable};
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
                merge_state_status: MergeStateStatus::Clean,
                updated_at: Timestamp::parse("2026-04-23T10:00:00Z").unwrap(),
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
            pull_request_metadata: PullRequestMetadata::Synced,
            attest_path: None,
            doc_review: DocReview::Synced,
            doc_review_attest_path: None,
            claude_review: ClaudeReview::NoActivity,
            claude_review_attest_path: None,
            closeout: Closeout::Synced,
            closeout_attest_path: None,
        }
    }
}
