//! Per-worker memory harness backed by [`ooda_state`].
//!
//! # Role
//!
//! One [`Recorder`] per `(slug, pr)` worker. Sole persistence
//! boundary for that worker's events (observations, decisions,
//! tool calls, actions, comments, waits, outcomes). Each worker
//! owns one [`ooda_state::RunWriter`] writing to a single
//! `runs/<run-id>/` directory under the shared state root.
//!
//! # Invariants
//!
//! - **Single writer per run**: one [`Recorder`] per
//!   `(slug, pr)`; internal mutation is serialized by
//!   `Arc<Mutex<_>>`.
//! - **Domain-agnostic on-disk layout**: paths under
//!   `<state-root>/runs/<run-id>/` carry no PR slug or PR number;
//!   PR identity is recorded only inside `RunStarted`'s `target`
//!   payload.
//! - **Worker isolation**: the tool-call sink is thread-local
//!   (see [`THREAD_RECORDER`]), so worker i's tool calls cannot
//!   land in worker j's ledger.
//! - **Best-effort writes**: persistence failures inside the
//!   recorder do not change the binary's `Outcome`. The
//!   `Recorder::open` boundary is the only point that surfaces an
//!   I/O error to the caller.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ooda_state::{
    BlobRef, EventBody, Result as StateResult, RunId, RunWriter, StateError, StateRoot,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::Decision;
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::compare::MergeBaseDelta;
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

/// Per-consumer input slice for [`Recorder::record_iteration`].
/// Each field declares a typed dep ref. The struct is the function
/// signature reified; it is not a god-struct in the
/// [`crate::orient::OrientedState`] sense — its scope is exactly
/// what this one consumer reads (dashboard inputs + the oriented
/// snapshot blob).
///
/// Field order mirrors `OrientedState` so the derived `Serialize`
/// impl produces a stable blob payload across recorder versions.
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

impl<'a> From<&'a crate::orient::OrientedState> for RecorderInputs<'a> {
    fn from(o: &'a crate::orient::OrientedState) -> Self {
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

thread_local! {
    /// Per-worker tool-call sink. Each PR-driving worker installs
    /// its own [`Recorder`] here.
    ///
    /// # Why thread-local, not process-global
    ///
    /// A process-global cell would last-writer-wins under
    /// concurrent suite execution, mis-routing tool-call records
    /// across PRs. Thread-locality is the minimal scope that
    /// preserves the worker-isolation invariant.
    static THREAD_RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
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
    /// Held for shape-parity with the mirrored consumers
    /// (`comment/post.rs`) across the 3 PR-side OODA binaries.
    /// The domain-agnostic [`ooda_state`] writer has no
    /// counterpart to the legacy concatenated-trace file; the
    /// field's value is ignored here.
    #[allow(dead_code)]
    pub legacy_trace: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct Recorder {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    state_root: StateRoot,
    writer: RunWriter,
    run_id: RunId,
    tool_sequence: u64,
    current_iteration: Option<u32>,
}

impl Recorder {
    // Signature held by-value for shape-parity with the mirrored
    // `comment/post.rs` test setup across the 3 PR-side OODA
    // binaries (see `scripts/check-mirror-invariants.sh`).
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn open(cfg: RecorderConfig) -> StateResult<Self> {
        let root_path = ooda_state::resolve_state_root(cfg.state_root.as_deref());
        let state_root = StateRoot::new(root_path)?;
        let run_id = RunId::generate();
        let mut writer = state_root.create_run(run_id.clone())?;
        writer.start(EventBody::RunStarted {
            domain: "pr".to_string(),
            target: json!({
                "forge": "github.com",
                "slug": cfg.slug.to_string(),
                "pr": cfg.pr.get(),
                "mode": cfg.mode,
                "max_iter": cfg.max_iter.get(),
                "status_comment": cfg.status_comment,
                "argv": std::env::args().collect::<Vec<_>>(),
                "cwd": std::env::current_dir()
                    .map_or_else(|_| "<unknown>".to_string(), |p| p.display().to_string()),
            }),
        })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                state_root,
                writer,
                run_id,
                tool_sequence: 0,
                current_iteration: None,
            })),
        })
    }

    /// Bind this [`Recorder`] to the current worker as its tool-call
    /// sink. Caller protocol: invoke once per worker before any tool
    /// call.
    pub(crate) fn install_process_recorder(&self) {
        THREAD_RECORDER.with(|cell| {
            *cell.borrow_mut() = Some(self.clone());
        });
    }

    pub(crate) fn set_iteration(&self, iteration: Option<u32>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.current_iteration = iteration;
        }
    }

    /// Opaque run identifier — same value the on-disk
    /// `runs/<run-id>/` directory uses. Surfaced in per-PR JSONL
    /// records so the suite-level output joins back to the per-run
    /// audit trail.
    pub(crate) fn run_id(&self) -> String {
        self.with_inner(|inner| inner.run_id.as_str().to_string())
            .unwrap_or_default()
    }

    /// Path to this run's directory. Used by the status-comment
    /// dedup file (per-run mutable state) and by tests.
    pub(crate) fn run_dir(&self) -> PathBuf {
        self.with_inner(|inner| {
            inner
                .state_root
                .path()
                .join("runs")
                .join(inner.run_id.as_str())
        })
        .unwrap_or_default()
    }

    /// Path the status-comment dedup state is read from / written
    /// to. Lives inside the run directory; dedup is per-run, not
    /// cross-run.
    pub(crate) fn dedup_path(&self) -> PathBuf {
        self.run_dir().join("status_comment_dedup.json")
    }

    /// Persist a handoff prompt body as a content-addressed blob
    /// and return its absolute path.
    ///
    /// # Postcondition
    ///
    /// On `Some(path)`: bytes are durable inside the run's blob
    /// store at `runs/<run-id>/blobs/<sha>.md`. The stderr handoff
    /// pointer (`see: <path>`) targets this file directly.
    ///
    /// # Failure modes
    ///
    /// Returns `None` when the write fails or no iteration is set.
    /// Caller must fall back to inline stderr emission so the
    /// prompt is never lost.
    pub(crate) fn write_handoff_md(&self, prompt: &str) -> Option<PathBuf> {
        let mut inner = self.inner.lock().ok()?;
        let iteration = inner.current_iteration?;
        let blob = inner.writer.write_blob(prompt.as_bytes(), "md").ok()?;
        let _ = inner.writer.append(EventBody::IterationHandoff {
            iteration,
            variant: "Handoff".to_string(),
            action_kind: "handoff_prompt".to_string(),
            blob: blob.clone(),
        });
        Some(blob_path(&inner, &blob))
    }

    pub(crate) fn write_trace_line(&self, line: &str) {
        self.best_effort(|inner| {
            inner.append_domain(
                "trace_line",
                json!({
                    "iteration": inner.current_iteration,
                    "line": line,
                }),
            )
        });
    }

    #[allow(clippy::too_many_lines)]
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
            inner.writer.append(EventBody::IterationObserved {
                iteration,
                blob: obs_blob,
            })?;

            let oriented_blob = inner.write_json_blob(inputs)?;
            inner.writer.append(EventBody::IterationOriented {
                iteration,
                blob: oriented_blob,
            })?;

            inner.writer.append(EventBody::IterationDecided {
                iteration,
                decision_kind: decision_kind(decision),
            })?;
            inner.append_domain(
                "decision_envelope",
                json!({
                    "iteration": iteration,
                    "candidate_count": candidates.len(),
                    "decision": decision_projection(decision),
                }),
            )?;
            inner.append_domain(
                "dashboard",
                json!({
                    "iteration": iteration,
                    "dashboard": serde_json::to_value(&dashboard).unwrap_or(Value::Null),
                }),
            )?;
            // Tier-grouped markdown projections written as blobs
            // so a downstream cockpit/agent reader can pull them
            // by `BlobRef` without re-rendering.
            let blockers_blob = inner
                .writer
                .write_blob(dashboard.render_blockers_md().as_bytes(), "md")?;
            inner.append_domain(
                "blockers_md",
                json!({ "iteration": iteration, "blob": blockers_blob }),
            )?;
            let next_blob = inner
                .writer
                .write_blob(dashboard.render_next_md().as_bytes(), "md")?;
            inner.append_domain(
                "next_md",
                json!({ "iteration": iteration, "blob": next_blob }),
            )?;
            Ok(())
        });
    }

    pub(crate) fn record_observe_start(&self, iteration: u32) {
        self.best_effort(|inner| {
            inner.append_domain("observe_started", json!({ "iteration": iteration }))
        });
    }

    pub(crate) fn record_observe_end(&self, iteration: u32, result: Result<(), String>) {
        self.best_effort(|inner| {
            let success = result.is_ok();
            inner.append_domain(
                "observe_finished",
                json!({
                    "iteration": iteration,
                    "success": success,
                    "error": result.err(),
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
            let blob = inner.write_json_blob(rendered)?;
            inner.append_domain(
                "status_comment_rendered",
                json!({
                    "iteration": iteration,
                    "summary": summary.into(),
                    "blob": blob,
                }),
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
            let blob = inner.write_json_blob(result)?;
            inner.append_domain(
                "status_comment_result",
                json!({
                    "iteration": iteration,
                    "summary": summary.into(),
                    "blob": blob,
                }),
            )
        });
    }

    pub(crate) fn record_action_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.append_domain(
                "action_started",
                json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                }),
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
            inner.writer.append(EventBody::IterationExecuted {
                iteration,
                action_kind: action.kind.name().to_string(),
            })?;
            inner.append_domain(
                "action_finished",
                json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                    "success": success,
                    "error": result.err(),
                }),
            )
        });
    }

    pub(crate) fn record_wait_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.append_domain(
                "wait_started",
                json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                }),
            )
        });
    }

    pub(crate) fn record_wait_end(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            let interval_ms = wait_interval_ms(action).unwrap_or(0);
            inner.writer.append(EventBody::IterationWaited {
                iteration,
                action_kind: action.kind.name().to_string(),
                interval_ms,
            })?;
            inner.append_domain(
                "wait_finished",
                json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                }),
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
            inner.append_domain(
                "outcome",
                json!({
                    "iteration": inner.current_iteration,
                    "outcome": outcome,
                    "exit_code": code,
                    "headline": headline,
                    "handoff_path": handoff_path.map(|p| p.display().to_string()),
                }),
            )?;
            inner.writer.halt(EventBody::RunHalted {
                outcome: outcome_variant_name(outcome).to_string(),
                exit_code: i32::from(code.as_u8()),
            })?;
            Ok(())
        });
    }

    fn best_effort(&self, f: impl FnOnce(&mut Inner) -> StateResult<()>) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = f(&mut inner);
        }
    }

    fn with_inner<T>(&self, f: impl FnOnce(&Inner) -> T) -> Option<T> {
        self.inner.lock().ok().map(|inner| f(&inner))
    }
}

impl Inner {
    fn write_json_blob<T: Serialize + ?Sized>(&self, value: &T) -> StateResult<BlobRef> {
        let bytes = serde_json::to_vec_pretty(value).map_err(StateError::from)?;
        self.writer.write_blob(&bytes, "json")
    }

    fn append_domain(&mut self, kind_suffix: &str, payload: Value) -> StateResult<()> {
        self.writer.append(EventBody::DomainSpecific {
            kind_suffix: kind_suffix.to_string(),
            payload,
        })
    }
}

fn blob_path(inner: &Inner, blob: &BlobRef) -> PathBuf {
    inner
        .state_root
        .path()
        .join("runs")
        .join(inner.run_id.as_str())
        .join("blobs")
        .join(format!("{}.{}", blob.sha, blob.ext))
}

/// Project a [`Decision`] onto a stable kind string the
/// domain-agnostic state schema records on `IterationDecided`.
fn decision_kind(decision: &Decision) -> String {
    match decision {
        Decision::Execute(_) => "Execute".to_string(),
        Decision::Halt(halt) => match halt {
            crate::decide::decision::DecisionHalt::Success => "Halt::Success".to_string(),
            crate::decide::decision::DecisionHalt::Terminal(t) => {
                format!("Halt::Terminal({t:?})")
            }
            crate::decide::decision::DecisionHalt::AgentNeeded(_) => {
                "Halt::AgentNeeded".to_string()
            }
            crate::decide::decision::DecisionHalt::HumanNeeded(_) => {
                "Halt::HumanNeeded".to_string()
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

fn action_projection(action: &Action) -> Value {
    json!({
        "kind": action.kind.name(),
        "target_effect": format!("{:?}", action.target_effect),
        "urgency": format!("{:?}", action.urgency),
        "blocker": action.blocker.to_string(),
        "effect": &action.effect,
    })
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

fn wait_interval_ms(action: &Action) -> Option<u64> {
    match &action.effect {
        crate::decide::action::ActionEffect::Wait { interval, .. } => {
            u64::try_from(interval.as_duration().as_millis()).ok()
        }
        _ => None,
    }
}

/// Bounded-token-set projection over [`Outcome`] used by the
/// `RunHalted` event variant. Mirrors the stderr / JSONL surface
/// names so an audit-reader can pivot from `events.jsonl` straight
/// to the SKILL.md outcome table.
fn outcome_variant_name(o: &Outcome) -> &'static str {
    match o {
        Outcome::DoneSucceeded => "DoneMerged",
        Outcome::StuckRepeated(_) => "StuckRepeated",
        Outcome::StuckCapReached(_) => "StuckCapReached",
        Outcome::HandoffHuman(_) => "HandoffHuman",
        Outcome::WouldAdvance(_) => "WouldAdvance",
        Outcome::HandoffAgent(_) => "HandoffAgent",
        Outcome::BinaryError(_) => "BinaryError",
        Outcome::Paused => "Paused",
        Outcome::DoneAborted => "DoneClosed",
        Outcome::UsageError(_) => "UsageError",
    }
}

// ── Tool-call guard ──────────────────────────────────────────────────

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
        inner.append_domain(
            "tool_call_started",
            json!({
                "call_id": call_id,
                "iteration": iteration,
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
            let stdout_blob = inner.writer.write_blob(&output.stdout, "bin")?;
            let stderr_blob = inner.writer.write_blob(&output.stderr, "bin")?;
            inner.append_domain(
                "tool_call_finished",
                json!({
                    "call_id": self.call_id,
                    "iteration": self.iteration,
                    "program": self.program,
                    "args": self.args,
                    "cwd": self.cwd,
                    "duration_ms": duration_ms,
                    "status_code": output.status.code(),
                    "success": output.status.success(),
                    "stdout_blob": stdout_blob,
                    "stderr_blob": stderr_blob,
                }),
            )
        });
    }

    pub(crate) fn finish_spawn_error(self, err: &io::Error) {
        let duration_ms = self.started.elapsed().as_millis();
        self.recorder.best_effort(|inner| {
            inner.append_domain(
                "tool_call_finished",
                json!({
                    "call_id": self.call_id,
                    "iteration": self.iteration,
                    "program": self.program,
                    "args": self.args,
                    "cwd": self.cwd,
                    "duration_ms": duration_ms,
                    "success": false,
                    "error": err.to_string(),
                }),
            )
        });
    }
}

fn next_tool_call_id_locked(recorder: &Recorder) -> Option<(String, Option<u32>)> {
    let mut inner = recorder.inner.lock().ok()?;
    inner.tool_sequence += 1;
    let call_id = format!("tc-{:06}", inner.tool_sequence);
    let iteration = inner.current_iteration;
    Some((call_id, iteration))
}

fn process_recorder() -> Option<Recorder> {
    THREAD_RECORDER.with(|cell| cell.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::ids::BlockerKey;
    use ooda_core::MidTier;
    use ooda_core::{HandoffPrompt, PollingInterval};

    fn sample_action(effect: ActionEffect) -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    /// Variant-wise golden assertion for the action JSON projection.
    /// Schema mismatch surfaces as a failing diff against the
    /// hand-written golden — adding a new `ActionEffect` variant
    /// requires updating the golden table here.
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
            "ooda-prs-recorder-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        ))
    }

    fn open_recorder(root: &Path) -> Recorder {
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
    fn open_writes_run_started_and_creates_live_marker() {
        let root = temp_root("open");
        let _ = std::fs::remove_dir_all(&root);
        let recorder = open_recorder(&root);
        let run_id = recorder.run_id();

        // Live marker present until halt.
        assert!(root.join("live").join(&run_id).exists());

        // events.jsonl contains a run_started line.
        let events =
            std::fs::read_to_string(root.join("runs").join(&run_id).join("events.jsonl")).unwrap();
        assert!(events.contains(r#""kind":"run_started""#), "{events}");
        assert!(events.contains(r#""domain":"pr""#), "{events}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_outcome_writes_run_halted_and_clears_live_marker() {
        let root = temp_root("halt");
        let _ = std::fs::remove_dir_all(&root);
        let recorder = open_recorder(&root);
        let run_id = recorder.run_id();

        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        assert!(!root.join("live").join(&run_id).exists());
        let events =
            std::fs::read_to_string(root.join("runs").join(&run_id).join("events.jsonl")).unwrap();
        assert!(events.contains(r#""kind":"run_halted""#), "{events}");
        assert!(events.contains(r#""outcome":"Paused""#), "{events}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_handoff_md_persists_body_as_blob() {
        let root = temp_root("handoff_md");
        let _ = std::fs::remove_dir_all(&root);
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));

        let body = "Rebase onto base\n\nContinuation line.";
        let path = recorder
            .write_handoff_md(body)
            .expect("write should succeed under temp root");

        let s = path.to_string_lossy();
        assert!(s.contains("/runs/"), "got {s}");
        assert!(s.contains("/blobs/"), "got {s}");
        assert!(s.ends_with(".md"), "got {s}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(root);
    }
}
