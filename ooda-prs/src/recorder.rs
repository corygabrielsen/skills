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
    BlobRef, DomainKind, EventBody, ObserveOutcome, OutcomeKind, PrDomain, Result as StateResult,
    RunId, RunWriter, StateError, StateRoot, domain_specific, terminal_event,
};

/// Error type returned by recorder path-resolving methods.
/// Aliased to [`StateError`] so the byte-identical
/// `comment/post.rs` mirror file can refer to a single
/// `crate::recorder::RecorderError` symbol that resolves to the
/// per-crate concrete error.
pub(crate) use ooda_state::StateError as RecorderError;
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
    slug: RepoSlug,
    pr: PullRequestNumber,
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
        // Best-effort: reclaim disk for live markers left behind by
        // crashed prior runs (PID-derived liveness).
        let _ = state_root.sweep_dead_markers();
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
                slug: cfg.slug,
                pr: cfg.pr,
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

    /// Per-PR dedup file for status comments. Lives outside the
    /// per-run tree because dedup is a cross-run invariant: a fresh
    /// run must observe prior runs' posted hashes.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the per-PR index directory
    /// cannot be created or the recorder mutex is poisoned. A
    /// silent fallback to a cwd-relative path would collapse
    /// distinct PRs onto a shared dedup file — fail loud instead.
    pub(crate) fn dedup_path(&self) -> StateResult<PathBuf> {
        Ok(self.pr_index_dir()?.join("status-comment-dedup.json"))
    }

    /// Per-PR advisory-lock sidecar target. Acquiring an
    /// [`ooda_core::FileLock`] on this path at the act-stage boundary
    /// serialises concurrent OODA invocations against the same PR:
    /// two drivers cannot dispatch a side-effecting action
    /// simultaneously. The path is per-`(slug, pr)`, not per-run, so
    /// drivers in distinct processes see the same lock.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the per-PR index directory
    /// cannot be created or the recorder mutex is poisoned. A
    /// silent fallback to a cwd-relative `.action.lock` would have
    /// all concurrent OODA invocations from the same cwd share one
    /// lock regardless of PR — the act-stage serialisation
    /// invariant would silently break.
    pub(crate) fn action_lock_path(&self) -> StateResult<PathBuf> {
        Ok(self.pr_index_dir()?.join(".action.lock"))
    }

    /// Per-PR sticky file recording the last remote head SHA the
    /// driver observed or caused. The branch-sync axis compares the
    /// current `headRefOid` against this sticky to detect drift; an
    /// unequal pair (with `pending = false`) is divergence (an
    /// out-of-band push). Path is per-`(slug, pr)`, parallel to
    /// [`Self::dedup_path`] and [`Self::action_lock_path`].
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] when the per-PR index directory
    /// cannot be created or the recorder mutex is poisoned. A
    /// silent fallback to a cwd-relative path would cross-pollinate
    /// drift signals between PRs — fail loud instead.
    pub(crate) fn last_seen_head_path(&self) -> StateResult<PathBuf> {
        Ok(self.pr_index_dir()?.join("last_seen_head.json"))
    }

    /// Resolve the per-PR index directory, creating it if needed.
    /// Shared core for [`Self::dedup_path`],
    /// [`Self::action_lock_path`], and [`Self::last_seen_head_path`].
    /// Each failure mode (mutex poison, `create_dir_all`) propagates
    /// as a typed error.
    fn pr_index_dir(&self) -> StateResult<PathBuf> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| StateError::Io(io::Error::other("recorder mutex poisoned")))?;
        pr_index_path(inner.state_root.path(), &inner.slug, inner.pr)
    }

    /// Persist a handoff prompt body as a content-addressed blob
    /// and return its absolute path.
    ///
    /// # Postcondition
    ///
    /// On `Ok(path)`: bytes are durable inside the run's blob
    /// store at `runs/<run-id>/blobs/<sha>.md` AND the
    /// `IterationHandoff` event referencing the blob is on disk
    /// in `events.jsonl`. The stderr handoff pointer (`see:
    /// <path>`) targets this file directly.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] on blob-write or event-append failure
    /// (disk full, mutex poison, no iteration set). Failed appends
    /// ARE surfaced: readers tailing `events.jsonl` depend on the
    /// event landing to observe the handoff. Silently discarding it
    /// would let the run go quiet without recording why it stopped.
    pub(crate) fn write_handoff_md(
        &self,
        prompt: &str,
        outcome: OutcomeKind,
        action_kind: &str,
    ) -> StateResult<PathBuf> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| StateError::Io(io::Error::other("recorder mutex poisoned")))?;
        let iteration = inner.current_iteration.ok_or_else(|| {
            StateError::Io(io::Error::other(
                "write_handoff_md called without a current iteration",
            ))
        })?;
        let blob = inner.writer.write_blob(prompt.as_bytes(), "md")?;
        inner.writer.append(EventBody::IterationHandoff {
            iteration,
            variant: outcome.variant_name().to_string(),
            action_kind: action_kind.to_string(),
            blob: blob.clone(),
        })?;
        Ok(blob_path(&inner, &blob))
    }

    pub(crate) fn write_trace_line(&self, line: &str) {
        self.best_effort(|inner| {
            inner.append_domain(
                DomainKind::TraceLine,
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

            let candidates_blob = inner.write_json_blob(candidates)?;
            inner.writer.append(domain_specific(
                DomainKind::IterationCandidates,
                json!({
                    "iteration": iteration,
                    "blob": candidates_blob,
                    "count": candidates.len(),
                }),
            ))?;

            let dashboard_blob = inner.write_json_blob(&dashboard)?;
            inner.writer.append(domain_specific(
                DomainKind::IterationDashboard,
                json!({
                    "iteration": iteration,
                    "blob": dashboard_blob,
                }),
            ))?;

            let decision_blob = inner.write_json_blob(decision)?;
            inner.writer.append(domain_specific(
                DomainKind::IterationDecisionEnvelope,
                json!({
                    "iteration": iteration,
                    "blob": decision_blob,
                    "decision": decision_projection(decision),
                }),
            ))?;

            inner.writer.append(EventBody::IterationDecided {
                iteration,
                decision_kind: decision_kind(decision),
            })?;
            Ok(())
        });
    }

    pub(crate) fn record_observe_start(&self, iteration: u32) {
        self.best_effort(|inner| {
            inner.append_domain(
                DomainKind::ObserveStarted,
                json!({ "iteration": iteration }),
            )
        });
    }

    // Held by-value for shape-parity with the mirrored consumers
    // across the 3 PR-side OODA binaries; `ObserveOutcome` owns
    // heap-allocated fields and the recorder reads them all.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn record_observe_end(&self, iteration: u32, outcome: ObserveOutcome) {
        let success = outcome.is_ok();
        let kind = outcome.kind();
        let error = outcome.error_message();
        let (scope, retry_after_secs) = match &outcome {
            ObserveOutcome::RateLimited {
                scope,
                retry_after_secs,
            } => (Some(scope.clone()), Some(*retry_after_secs)),
            _ => (None, None),
        };
        self.best_effort(|inner| {
            inner.append_domain(
                DomainKind::ObserveFinished,
                json!({
                    "iteration": iteration,
                    "kind": kind,
                    "success": success,
                    "error": error,
                    "rate_limit_scope": scope,
                    "rate_limit_retry_after_secs": retry_after_secs,
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
                DomainKind::StatusCommentRendered,
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
                DomainKind::StatusCommentResult,
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
                DomainKind::ActionStarted,
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
            // Atomicity-class C9: `action_finished` (carries
            // success+error) precedes `IterationExecuted`. A
            // crash between leaves the truthful failure event on
            // disk rather than a bare success marker that would
            // mislead the audit chain.
            inner.append_domain(
                DomainKind::ActionFinished,
                json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                    "success": success,
                    "error": result.err(),
                }),
            )?;
            // `IterationExecuted` is the typed audit-trail marker for
            // non-wait actions. Wait actions emit their own
            // `IterationWaited` from `record_wait_end`; gating here
            // keeps the two event streams disjoint.
            if !action.effect.is_wait() {
                inner.writer.append(EventBody::IterationExecuted {
                    iteration,
                    action_kind: action.kind.name().to_string(),
                    success,
                })?;
            }
            Ok(())
        });
    }

    pub(crate) fn record_wait_start(&self, iteration: u32, action: &Action) {
        self.best_effort(|inner| {
            inner.append_domain(
                DomainKind::WaitStarted,
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
                DomainKind::WaitFinished,
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
        let kind = outcome_kind(outcome);
        let last_action = stall_action_kind(outcome);
        self.best_effort(|inner| {
            let outcome_blob = inner.write_json_blob(outcome)?;
            inner.append_domain(
                DomainKind::Outcome,
                json!({
                    "exit_code": code,
                    "headline": headline,
                    "handoff_path": handoff_path.map(|p| p.display().to_string()),
                    "blob": outcome_blob,
                }),
            )?;
            inner.writer.halt(terminal_event(
                &PrDomain,
                kind,
                i32::from(code.as_u8()),
                last_action.as_deref(),
            ))?;
            Ok(())
        });
    }

    fn best_effort(&self, f: impl FnOnce(&mut Inner) -> StateResult<()>) {
        match self.inner.lock() {
            Ok(mut inner) => {
                if let Err(e) = f(&mut inner) {
                    eprintln!("ooda recorder: append failed: {e}");
                }
            }
            Err(_) => {
                eprintln!("ooda recorder: mutex poisoned; event dropped");
            }
        }
    }

    fn with_inner<T>(&self, f: impl FnOnce(&Inner) -> T) -> Option<T> {
        self.inner.lock().ok().map(|inner| f(&inner))
    }
}

impl Inner {
    fn write_json_blob<T: Serialize + ?Sized>(&mut self, value: &T) -> StateResult<BlobRef> {
        let bytes = serde_json::to_vec_pretty(value).map_err(StateError::from)?;
        self.writer.write_blob(&bytes, "json")
    }

    fn append_domain(&mut self, kind: DomainKind, payload: Value) -> StateResult<()> {
        self.writer.append(domain_specific(kind, payload))
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
        "description": action.rendered_payload(),
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
        "description": handoff.prompt.to_string(),
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

/// Project an [`Outcome`] onto its [`OutcomeKind`] discriminant.
/// Strips the payload; the recorder uses this to drive the
/// per-domain `outcome_token` table without coupling `ooda-state`
/// to `ooda-core`.
pub(crate) fn outcome_kind(outcome: &Outcome) -> OutcomeKind {
    match outcome {
        Outcome::DoneSucceeded => OutcomeKind::DoneSucceeded,
        Outcome::DoneAborted => OutcomeKind::DoneAborted,
        Outcome::Paused => OutcomeKind::Paused,
        Outcome::WouldAdvance(_) => OutcomeKind::WouldAdvance,
        Outcome::HandoffHuman(_) => OutcomeKind::HandoffHuman,
        Outcome::HandoffAgent(_) => OutcomeKind::HandoffAgent,
        Outcome::StuckRepeated(_) => OutcomeKind::StuckRepeated,
        Outcome::StuckCapReached(_) => OutcomeKind::StuckCapReached,
        Outcome::UsageError(_) => OutcomeKind::UsageError,
        Outcome::BinaryError(_) => OutcomeKind::BinaryError,
        Outcome::SignalInterrupted { .. } => OutcomeKind::SignalInterrupted,
    }
}

/// Repeating action's kind name for stall-class outcomes, used as
/// the `last_action` payload on `RunStalled` / `RunCapReached`.
fn stall_action_kind(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::StuckRepeated(action) | Outcome::StuckCapReached(action) => {
            Some(action.kind.name().to_string())
        }
        _ => None,
    }
}

/// Per-PR index directory: a stable per-`(slug, pr)` location for
/// cross-run state (e.g. status-comment dedup). Lives under
/// `<state-root>/index/pr/<owner>/<repo>/<pr>/`, parallel to the
/// run tree at `<state-root>/runs/`.
///
/// Surfaces `create_dir_all` failures: the prior best-effort
/// `let _ = fs::create_dir_all(...)` swallowed perms / disk-full
/// errors and returned a non-existent directory; downstream
/// callers that joined a sentinel filename onto it (lock,
/// dedup, sticky) ended up with paths that could not be created
/// and the caller saw a confusing "permission denied" on a path
/// that looked correct.
fn pr_index_path(root: &Path, slug: &RepoSlug, pr: PullRequestNumber) -> StateResult<PathBuf> {
    let dir = root
        .join("index")
        .join("pr")
        .join(slug.owner().as_str())
        .join(slug.repo().as_str())
        .join(pr.to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
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
            DomainKind::ToolCallStarted,
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
                DomainKind::ToolCallFinished,
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
                DomainKind::ToolCallFinished,
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
            .write_handoff_md(body, OutcomeKind::HandoffHuman, "Rebase")
            .expect("write should succeed under temp root");

        let s = path.to_string_lossy();
        assert!(s.contains("/runs/"), "got {s}");
        assert!(s.contains("/blobs/"), "got {s}");
        assert!(s.ends_with(".md"), "got {s}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_handoff_md_returns_err_when_append_fails() {
        // Site 5 invariant: failed `IterationHandoff` append
        // surfaces as Err rather than silently dropping the event.
        let root = temp_root("handoff_err");
        let _ = std::fs::remove_dir_all(&root);
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));
        // Force the blob/append step to fail by unlinking the run
        // tree between `set_iteration` and the call.
        let runs_dir = root.join("runs");
        std::fs::remove_dir_all(&runs_dir).unwrap();
        let result = recorder.write_handoff_md("body", OutcomeKind::HandoffHuman, "Rebase");
        assert!(result.is_err(), "got {result:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn action_lock_path_returns_err_when_index_dir_uncreatable() {
        // Site 1 invariant: when the per-PR index directory cannot
        // be created, `action_lock_path` MUST return Err rather
        // than degrading to a cwd-relative `.action.lock`.
        let root = temp_root("action_lock_err");
        let _ = std::fs::remove_dir_all(&root);
        let recorder = open_recorder(&root);
        // Place a regular file where the index directory needs to
        // be — `create_dir_all` will return Err.
        let blocker = root.join("index").join("pr").join("example");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, b"not-a-directory").unwrap();
        assert!(recorder.action_lock_path().is_err());
        assert!(recorder.dedup_path().is_err());
        assert!(recorder.last_seen_head_path().is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
