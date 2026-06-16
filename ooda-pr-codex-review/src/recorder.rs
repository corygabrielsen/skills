//! Facade over `ooda_state` for the per-PR (codex-review-capable)
//! OODA loop.
//!
//! # Role
//!
//! Sole persistence boundary for the per-PR loop with optional codex
//! axis. Translates the binary's domain vocabulary (slug, PR,
//! actions, decisions, outcomes, status comment, codex review) into
//! the domain-neutral `ooda_state` event protocol — `RunStarted`
//! with `domain="pr"`, per-iteration `IterationObserved` /
//! `IterationOriented` / `IterationDecided` events with blob refs,
//! `IterationExecuted` / `IterationWaited` for action completions,
//! `DomainSpecific` for everything else.
//!
//! # Invariants
//!
//! - **Single writer per run**: one `RunWriter` per `Recorder`;
//!   mutation serialized via `Arc<Mutex<_>>`. Concurrent calls to
//!   the same recorder are safe; concurrent recorders against the
//!   same `<run-id>` are rejected at `start` time by
//!   `ooda_state`.
//! - **Append-only event log**: every recorder call appends to
//!   `events.jsonl` via `RunWriter::append`; no record is ever
//!   rewritten.
//! - **Content-addressed blobs**: per-iteration artifacts (observed
//!   bundle, oriented snapshot, candidates, decision, dashboard,
//!   handoff body, tool-call stdout/stderr) are written via
//!   `RunWriter::write_blob`; events carry only `BlobRef` handles.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};

use ooda_state::{
    BlobRef, DecisionKind, Domain, DomainKind, EventBody, ObserveOutcome, OutcomeKind, PrDomain,
    RunWriter, StateRoot, domain_specific, resolve_state_root, terminal_event,
};

use crate::dashboard::Dashboard;
use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt, Terminal};
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

/// Per-consumer input slice for [`Recorder::record_iteration`]. Each
/// field declares a typed dep ref. Field order mirrors `OrientedState`
/// so the derived `Serialize` impl produces byte-identical JSON for
/// the `oriented` snapshot blob.
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

static PROCESS_RECORDER: OnceLock<Mutex<Option<Recorder>>> = OnceLock::new();

#[derive(Debug)]
pub enum RecorderError {
    Io(io::Error),
    State(ooda_state::StateError),
    Json(serde_json::Error),
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::State(e) => write!(f, "{e}"),
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

impl From<ooda_state::StateError> for RecorderError {
    fn from(e: ooda_state::StateError) -> Self {
        Self::State(e)
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

/// Per-invocation codex-axis bounds, surfaced into the `RunStarted`
/// target payload. `None` ⇔ codex axis disabled.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexReviewSnapshot {
    pub floor: String,
    pub ceiling: String,
    pub n: u32,
}

/// Per-recorder configuration. Field set is byte-identical with the
/// mirror set's `comment/post.rs` test fixture; the `legacy_trace`
/// field is held purely to satisfy that shared literal and is
/// otherwise ignored. Per-binary extras (codex-review snapshot) flow
/// in via [`Recorder::record_codex_review_config`] after construction
/// rather than expanding this struct.
#[derive(Debug, Clone)]
pub(crate) struct RecorderConfig {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub mode: RunMode,
    pub max_iter: std::num::NonZeroU32,
    pub status_comment: bool,
    pub state_root: Option<PathBuf>,
    #[allow(dead_code)] // mirror-shape compat; see struct doc
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
    tool_sequence: u64,
    current_iteration: Option<u32>,
}

impl Recorder {
    pub(crate) fn open(cfg: RecorderConfig) -> Result<Self, RecorderError> {
        let root_path = resolve_state_root(cfg.state_root.as_deref());
        let state_root = StateRoot::new(&root_path)?;
        // Best-effort: reclaim disk for live markers left behind by
        // crashed prior runs (PID-derived liveness).
        let _ = state_root.sweep_dead_markers();
        let run_id = ooda_state::RunId::generate();
        let mut writer = state_root.create_run(run_id)?;

        let target = json!({
            "slug": cfg.slug.to_string(),
            "pr": u64::from(cfg.pr),
            "mode": cfg.mode,
            "max_iter": cfg.max_iter.get(),
            "status_comment": cfg.status_comment,
        });
        writer.start(EventBody::RunStarted {
            domain: PrDomain.name().to_string(),
            target,
        })?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                slug: cfg.slug,
                pr: cfg.pr,
                state_root,
                writer,
                tool_sequence: 0,
                current_iteration: None,
            })),
        })
    }

    /// Emit a `domain_specific:codex_review_config` event capturing
    /// the per-invocation codex-axis bounds. `None` records the
    /// disabled state explicitly so downstream consumers can
    /// distinguish "axis disabled" from "axis configuration absent
    /// from event log".
    pub(crate) fn record_codex_review_config(&self, snapshot: Option<&CodexReviewSnapshot>) {
        self.best_effort(|inner| {
            let payload = match snapshot {
                Some(s) => json!({ "enabled": true, "snapshot": s }),
                None => json!({ "enabled": false }),
            };
            inner.append_domain_raw("codex_review_config", payload)
        });
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

    /// Opaque run identifier — same value the on-disk
    /// `runs/<run-id>/` directory uses. Surfaced in advisory stderr
    /// prefixes so a human watching concurrent OODA loops can join a
    /// warning line back to its run audit trail. Returns the empty
    /// string on mutex poison; callers omit the `run=` field when
    /// empty.
    pub(crate) fn run_id(&self) -> String {
        match self.inner.lock() {
            Ok(inner) => inner.writer.run_id().as_str().to_string(),
            Err(_) => String::new(),
        }
    }

    /// Cross-run PR-keyed workspace root: `<state-root>/workspaces/
    /// pr-codex-review/<slug>/<pr>/`. Act-side state (codex spawn
    /// directories, comment dedup file) lives under this path,
    /// outside the run-opaque `runs/` tree.
    ///
    /// # Errors
    ///
    /// Returns [`RecorderError`] when the recorder mutex is
    /// poisoned. A silent fallback to an empty `PathBuf` would let
    /// downstream callers join sentinel filenames onto a
    /// cwd-relative path and collide across distinct PRs.
    pub(crate) fn pr_workspace_root(&self) -> Result<PathBuf, RecorderError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| RecorderError::Io(io::Error::other("recorder mutex poisoned")))?;
        Ok(pr_workspace_root(
            inner.state_root.path(),
            &inner.slug,
            inner.pr,
        ))
    }

    /// Path of the status-comment dedup file. Cross-run, PR-keyed —
    /// lives under the workspace root, not inside a per-run dir.
    ///
    /// # Errors
    ///
    /// Returns [`RecorderError`] when [`Self::pr_workspace_root`]
    /// fails. A cwd-relative fallback would silently collapse
    /// distinct PRs onto a shared dedup file.
    pub(crate) fn dedup_path(&self) -> Result<PathBuf, RecorderError> {
        Ok(self.pr_workspace_root()?.join("status-comment.json"))
    }

    /// Per-PR advisory-lock sidecar target. Acquiring an
    /// [`ooda_core::FileLock`] on this path at the act-stage boundary
    /// serialises concurrent OODA invocations against the same PR:
    /// two drivers cannot dispatch a side-effecting action
    /// simultaneously. The path is per-`(slug, pr)`, not per-run, so
    /// drivers in distinct processes see the same lock.
    ///
    /// Distinct from the workspace-level codex `.lock` held
    /// FD-tied by [`crate::act::CodexActContext`] — the codex lock
    /// excludes concurrent invocations from sharing the spawn
    /// directories; this lock serialises the act-stage dispatch
    /// itself.
    ///
    /// # Errors
    ///
    /// Returns [`RecorderError`] when [`Self::pr_workspace_root`]
    /// fails. A cwd-relative fallback would have all concurrent
    /// OODA invocations from the same cwd share one lock
    /// regardless of PR.
    pub(crate) fn action_lock_path(&self) -> Result<PathBuf, RecorderError> {
        Ok(self.pr_workspace_root()?.join(".action.lock"))
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
    /// Returns [`RecorderError`] when [`Self::pr_workspace_root`]
    /// fails. A cwd-relative fallback would cross-pollinate drift
    /// signals between PRs.
    pub(crate) fn last_seen_head_path(&self) -> Result<PathBuf, RecorderError> {
        Ok(self.pr_workspace_root()?.join("last_seen_head.json"))
    }

    /// Persist a handoff prompt body as a content-addressed blob and
    /// return its absolute on-disk path. Callers point stderr's
    /// `see:` line at this path; the file's size is observable via
    /// `stat`, decoupling consumption from any streaming truncation
    /// budget.
    ///
    /// `outcome` names which handoff variant is in flight
    /// ([`OutcomeKind::HandoffHuman`] or [`OutcomeKind::HandoffAgent`]);
    /// the emitted [`EventBody::IterationHandoff`] carries the
    /// outcome's wire variant name so the reader can pivot on the
    /// same token the stderr header uses.
    ///
    /// # Errors
    ///
    /// Returns [`RecorderError`] on blob-write or event-append
    /// failure. A failed append IS surfaced: readers tailing
    /// `events.jsonl` (cockpit, projection, run reconciler) depend
    /// on the event landing to observe the handoff. Silently
    /// discarding it would break the SKILL.md §Surface-to-user
    /// contract — the run would go quiet without recording why it
    /// stopped.
    pub(crate) fn write_handoff_md(
        &self,
        prompt: &str,
        outcome: OutcomeKind,
        action_kind: &str,
    ) -> Result<PathBuf, RecorderError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| RecorderError::Io(io::Error::other("recorder mutex poisoned")))?;
        let iteration = inner.current_iteration.ok_or_else(|| {
            RecorderError::Io(io::Error::other(
                "write_handoff_md called without a current iteration",
            ))
        })?;
        let blob = inner.writer.write_blob(prompt.as_bytes(), "md")?;
        let path = blob_path(
            inner.state_root.path(),
            inner.writer.run_id().as_str(),
            &blob,
        );
        inner.writer.append(EventBody::IterationHandoff {
            iteration,
            variant: outcome.variant_name().to_string(),
            action_kind: action_kind.to_string(),
            blob,
        })?;
        Ok(path)
    }

    pub(crate) fn write_trace_line(&self, line: &str) {
        self.best_effort(|inner| {
            inner.append_domain(DomainKind::TraceLine, json!({ "line": line }))
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
            inner.append_domain(
                DomainKind::IterationCandidates,
                json!({
                    "iteration": iteration,
                    "blob": candidates_blob,
                    "count": candidates.len(),
                }),
            )?;

            let dashboard_blob = inner.write_json_blob(&dashboard)?;
            inner.append_domain(
                DomainKind::IterationDashboard,
                json!({
                    "iteration": iteration,
                    "blob": dashboard_blob,
                }),
            )?;

            let decision_blob = inner.write_json_blob(decision)?;
            inner.append_domain(
                DomainKind::IterationDecisionEnvelope,
                json!({
                    "iteration": iteration,
                    "blob": decision_blob,
                    "decision": decision_projection(decision),
                }),
            )?;

            inner.writer.append(EventBody::IterationDecided {
                iteration,
                decision_kind: decision_kind_token(decision),
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
        let summary = summary.into();
        self.best_effort(|inner| {
            let blob = inner.write_json_blob(rendered)?;
            inner.append_domain(
                DomainKind::StatusCommentRendered,
                json!({
                    "iteration": iteration,
                    "summary": summary,
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
        let summary = summary.into();
        self.best_effort(|inner| {
            let blob = inner.write_json_blob(result)?;
            inner.append_domain(
                DomainKind::StatusCommentResult,
                json!({
                    "iteration": iteration,
                    "summary": summary,
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
            let error = result.err();
            inner.append_domain(
                DomainKind::ActionFinished,
                json!({
                    "iteration": iteration,
                    "action": action_projection(action),
                    "success": success,
                    "error": error,
                }),
            )?;
            // `IterationExecuted` is the typed audit-trail marker for
            // non-wait actions. Wait actions emit their own
            // `IterationWaited` from `record_wait_end`; gating here
            // keeps the two event streams disjoint. A failed Full
            // action still emits — its `success: false` field
            // distinguishes it from a clean completion.
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
            let interval_ms = match &action.effect {
                crate::decide::action::ActionEffect::Wait { interval, .. } => {
                    u64::try_from(interval.as_duration().as_millis()).unwrap_or(u64::MAX)
                }
                _ => 0,
            };
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
            )?;
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
        let kind = outcome_kind(outcome);
        let last_action = stall_action_kind(outcome);
        self.best_effort(|inner| {
            let outcome_blob = inner.write_json_blob(outcome)?;
            inner.append_domain(
                DomainKind::Outcome,
                json!({
                    "exit_code": code,
                    "headline": headline,
                    "handoff_path": handoff_path.map(Path::to_path_buf),
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

    fn best_effort(&self, f: impl FnOnce(&mut Inner) -> Result<(), RecorderError>) {
        match self.inner.lock() {
            Ok(mut inner) => {
                if let Err(e) = f(&mut inner) {
                    // Loop-identity prefix lets a human watching ≥2
                    // concurrent OODA loops disambiguate which run
                    // emitted this advisory. Shape matches the
                    // main-loop helper `loop_prefix` in main.rs.
                    eprintln!(
                        "[ooda-pr-codex-review {}#{} run={}] recorder: append failed: {e}",
                        inner.slug,
                        inner.pr,
                        inner.writer.run_id().as_str(),
                    );
                }
            }
            Err(_) => {
                // Mutex poisoned: slug/pr/run-id unrecoverable here.
                // Emit the binary tag alone so the line is still
                // attributable to ooda-pr-codex-review.
                eprintln!("[ooda-pr-codex-review] recorder: mutex poisoned; event dropped");
            }
        }
    }
}

impl Inner {
    fn append_domain(&mut self, kind: DomainKind, payload: Value) -> Result<(), RecorderError> {
        self.writer.append(domain_specific(kind, payload))?;
        Ok(())
    }

    /// Emit a `DomainSpecific` event with a raw `kind_suffix`
    /// literal. Reserved for per-binary extras outside the shared
    /// [`DomainKind`] vocabulary (e.g. `codex_review_config`); calls
    /// using a value that belongs in `DomainKind` are a soft
    /// drift signal the mirror-check script catches.
    fn append_domain_raw(
        &mut self,
        kind_suffix: &str,
        payload: Value,
    ) -> Result<(), RecorderError> {
        self.writer.append(EventBody::DomainSpecific {
            kind_suffix: kind_suffix.to_string(),
            payload,
        })?;
        Ok(())
    }

    fn write_json_blob<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<BlobRef, RecorderError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        Ok(self.writer.write_blob(&bytes, "json")?)
    }

    fn next_tool_call_id(&mut self) -> String {
        self.tool_sequence += 1;
        format!("tc-{:06}", self.tool_sequence)
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

/// Stable token for a `Decision` variant, suitable for the
/// `decision_kind` field on `IterationDecided`.
///
/// The wire-string literals live on [`DecisionKind`] in
/// `ooda_state::tokens` — every recorder (PR trio + codex-review)
/// routes through that enum so the wire shape cannot drift between
/// binaries.
fn decision_kind_token(decision: &Decision) -> String {
    decision_kind_discriminant(decision).as_str().to_string()
}

fn decision_kind_discriminant(decision: &Decision) -> DecisionKind {
    match decision {
        Decision::Execute(_) => DecisionKind::Execute,
        Decision::Halt(DecisionHalt::Success) => DecisionKind::HaltSuccess,
        Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded)) => {
            DecisionKind::HaltTerminalSucceeded
        }
        Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted)) => {
            DecisionKind::HaltTerminalAborted
        }
        Decision::Halt(DecisionHalt::AgentNeeded(_)) => DecisionKind::HaltAgentNeeded,
        Decision::Halt(DecisionHalt::HumanNeeded(_)) => DecisionKind::HaltHumanNeeded,
    }
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

/// Cross-run PR-keyed workspace path. Lives under the state root but
/// outside `runs/` and `live/`; the run-opaque core does not know
/// about this directory.
pub(crate) fn pr_workspace_root(
    state_root: &Path,
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> PathBuf {
    state_root
        .join("workspaces")
        .join("pr-codex-review")
        .join(slug.owner().as_str())
        .join(slug.repo().as_str())
        .join(pr.to_string())
}

fn blob_path(state_root: &Path, run_id: &str, blob: &BlobRef) -> PathBuf {
    ooda_state::blob_path(state_root, run_id, blob)
}

// ── Tool-call surfaces ──────────────────────────────────────────────

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
                "iteration": iteration,
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
        let status_code = output.status.code();
        let success = output.status.success();
        self.recorder.best_effort(|inner| {
            let stdout_blob = inner.writer.write_blob(&output.stdout, "bin")?;
            let stderr_blob = inner.writer.write_blob(&output.stderr, "bin")?;
            inner.append_domain(
                DomainKind::ToolCallFinished,
                json!({
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
            )
        });
    }

    pub(crate) fn finish_spawn_error(self, err: &io::Error) {
        let duration_ms = self.started.elapsed().as_millis();
        let error_text = err.to_string();
        self.recorder.best_effort(|inner| {
            inner.append_domain(
                DomainKind::ToolCallFinished,
                json!({
                    "iteration": self.iteration,
                    "call_id": self.call_id,
                    "program": self.program,
                    "args": self.args,
                    "cwd": self.cwd,
                    "duration_ms": duration_ms,
                    "success": false,
                    "error": error_text,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::ids::BlockerKey;
    use ooda_core::MidTier;
    use ooda_core::PollingInterval;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ooda-pr-codex-review-recorder-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sample_action(effect: ActionEffect) -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    fn open_recorder(root: &Path) -> Recorder {
        Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("example/widgets").unwrap(),
            pr: PullRequestNumber::new(7).unwrap(),
            mode: RunMode::Inspect,
            max_iter: std::num::NonZeroU32::new(1).unwrap(),
            status_comment: false,
            state_root: Some(root.to_path_buf()),
            legacy_trace: None,
        })
        .unwrap()
    }

    fn read_events(root: &Path) -> String {
        let runs = root.join("runs");
        let first = std::fs::read_dir(&runs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::read_to_string(first.join("events.jsonl")).unwrap()
    }

    #[test]
    fn open_writes_run_started_and_creates_live_marker() {
        let root = temp_root("open");
        let recorder = open_recorder(&root);
        // Live marker present mid-run, absent after halt.
        let live_dir = root.join("live");
        assert!(live_dir.is_dir());
        let live_count = std::fs::read_dir(&live_dir).unwrap().count();
        assert_eq!(live_count, 1);

        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        let live_count_after = std::fs::read_dir(&live_dir).unwrap().count();
        assert_eq!(live_count_after, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_started_and_halted_events_are_present() {
        let root = temp_root("start-halt");
        let recorder = open_recorder(&root);
        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        let events_text = read_events(&root);
        assert!(
            events_text.contains(r#""kind":"run_started""#),
            "{events_text}"
        );
        assert!(events_text.contains(r#""domain":"pr""#), "{events_text}");
        assert!(
            events_text.contains(r#""kind":"run_halted""#),
            "{events_text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_handoff_md_returns_blob_path() {
        let root = temp_root("handoff");
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));

        let body = "Rebase onto base\n\nContinuation line.";
        let path = recorder
            .write_handoff_md(body, OutcomeKind::HandoffHuman, "Rebase")
            .unwrap();
        assert!(path.exists(), "handoff blob path: {path:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_handoff_md_returns_err_when_append_fails() {
        // Site 5 invariant: failed `IterationHandoff` append
        // surfaces as Err rather than silently dropping the event.
        //
        // Failure injection: replace the run directory with a
        // regular file. `write_atomic` and `open_secure_append` both
        // try to materialize the run subtree (`blobs/`,
        // `events.jsonl`) under that path; `secure_create_dir_all`
        // refuses to walk through a non-directory and the call
        // surfaces an IO error.
        let root = temp_root("handoff-err");
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));
        let run_subtree = std::fs::read_dir(root.join("runs"))
            .unwrap()
            .next()
            .expect("recorder must have created a run dir")
            .unwrap()
            .path();
        std::fs::remove_dir_all(&run_subtree).unwrap();
        std::fs::write(&run_subtree, b"not-a-directory").unwrap();
        let result = recorder.write_handoff_md("body", OutcomeKind::HandoffHuman, "Rebase");
        assert!(result.is_err(), "got {result:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn record_action_end_emits_iteration_executed_on_success() {
        let root = temp_root("exec");
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));

        let action = sample_action(ActionEffect::Full {
            log: "Mark PR ready".into(),
            upstream: ooda_core::UpstreamConsistency::Sync,
        });
        recorder.record_action_end(1, &action, Ok(()));
        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        let events_text = read_events(&root);
        assert!(
            events_text.contains(r#""kind":"iteration_executed""#),
            "{events_text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn record_wait_end_emits_iteration_waited_with_interval_ms() {
        let root = temp_root("wait");
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));

        let action = sample_action(ActionEffect::Wait {
            interval: PollingInterval::from_secs(30),
            log: "Waiting".into(),
        });
        recorder.record_wait_end(1, &action);
        recorder.record_outcome(&Outcome::Paused, ExitCode::Paused, "Paused", None);

        let events_text = read_events(&root);
        assert!(
            events_text.contains(r#""kind":"iteration_waited""#),
            "{events_text}",
        );
        assert!(
            events_text.contains(r#""interval_ms":30000"#),
            "{events_text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn outcome_stuck_repeated_emits_run_stalled() {
        let root = temp_root("stalled");
        let recorder = open_recorder(&root);
        recorder.set_iteration(Some(1));

        let action = sample_action(ActionEffect::Full {
            log: "stalled".into(),
            upstream: ooda_core::UpstreamConsistency::Sync,
        });
        recorder.record_outcome(
            &Outcome::StuckRepeated(Box::new(action)),
            ExitCode::StuckRepeated,
            "StuckRepeated",
            None,
        );

        let events_text = read_events(&root);
        assert!(
            events_text.contains(r#""kind":"run_stalled""#),
            "{events_text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn action_lock_path_returns_err_on_mutex_poison() {
        // Site 1 invariant: when the recorder's mutex is poisoned,
        // the path-resolving methods MUST return Err rather than
        // degrading to an empty `PathBuf` (which `unwrap_or_default`
        // produced and which then joined to a cwd-relative sentinel
        // filename, silently collapsing distinct PRs onto a shared
        // lock / dedup / sticky file).
        //
        // Poison the recorder's internal mutex by holding it while
        // a thread panics.
        let root = temp_root("action-lock-poison");
        let recorder = open_recorder(&root);
        let handle = {
            let recorder = recorder.clone();
            std::thread::spawn(move || {
                let _guard = recorder.inner.lock().unwrap();
                panic!("intentionally poison mutex");
            })
        };
        let _ = handle.join();
        assert!(
            recorder.action_lock_path().is_err(),
            "action_lock_path must surface mutex poison",
        );
        assert!(recorder.dedup_path().is_err());
        assert!(recorder.last_seen_head_path().is_err());
        assert!(recorder.pr_workspace_root().is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pr_workspace_root_is_pr_keyed() {
        let slug = RepoSlug::parse("acme/widgets").unwrap();
        let pr = PullRequestNumber::new(42).unwrap();
        let path = pr_workspace_root(Path::new("/state"), &slug, pr);
        assert_eq!(
            path,
            PathBuf::from("/state/workspaces/pr-codex-review/acme/widgets/42")
        );
    }
}
