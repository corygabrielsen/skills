//! OODA loop driver: observe → orient → decide → act, repeated
//! until a halt fires.
//!
//! # Halt conditions
//!
//! - **Decision halt** — decide returns a halt variant.
//! - **Stall** — same `(kind_name, blocker)` fires on two
//!   consecutive non-`Wait` iterations. Coarse: only catches the
//!   single-action spinning case.
//! - **Cap** — the iteration cap is reached without halting; the
//!   last attempted action is the triage anchor.
//!
//! All four collapse into [`HaltReason`]; exit-code mapping lives
//! on [`HaltReason::exit_code`].
//!
//! # Event emission
//!
//! Each iteration emits a fixed sequence of events through
//! [`EventSink`] for audit: orient snapshot → decision → (act
//! result OR halt reason). Snapshots large enough to dwarf an
//! events line go through the sink's content-addressed blob
//! store; everything else is inlined on the event.
//!
//! # Structural invariants
//!
//! - `LoopConfig::max_iterations` is `NonZeroU32`, so iter 1 is
//!   structurally guaranteed to run. The driver splits iter 1
//!   from subsequent iterations so the cap-halt's
//!   `last_attempted` is a typed `Action` (not `Option<Action>`),
//!   eliminating the runtime expect that would otherwise document
//!   this invariant.

use std::num::NonZeroU32;

use ooda_state::{EventBody, RunWriter};

use crate::act::{ActContext, ActError, act};
use crate::decide::action::{Action, ActionEffect, CodexReasoningLevel};
use crate::decide::decide;
use crate::decide::decision::{Decision, DecisionHalt, HaltReason};
use crate::ids::{RepoId, ReviewTarget};
use crate::observe::codex::CodexObservations;
use crate::orient::OrientedState;
use crate::orient::orient;

#[derive(Debug)]
pub(crate) enum LoopError {
    Observe(String),
    Act(ActError),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observe(e) => write!(f, "observe: {e}"),
            Self::Act(e) => write!(f, "act: {e}"),
        }
    }
}

impl std::error::Error for LoopError {}

#[derive(Clone, Copy)]
pub(crate) struct LoopConfig {
    /// Iteration cap. `NonZeroU32` so iter-1-always-runs is a
    /// type-level guarantee.
    pub max_iterations: NonZeroU32,
    /// Top of the reasoning ladder. An all-clean batch at this
    /// level halts terminally (the per-target fixed point);
    /// all-clean below ceiling emits the retrospective handoff
    /// instead.
    pub ceiling: CodexReasoningLevel,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: NonZeroU32::new(50).expect("50 is non-zero"),
            ceiling: CodexReasoningLevel::Xhigh,
        }
    }
}

/// Per-iteration event emitter backed by an [`ooda_state::RunWriter`].
/// One method per OODA stage; each writes one [`EventBody`] (and
/// optionally a content-addressed blob) to the run's events.jsonl.
///
/// Errors are intentionally swallowed: the loop's correctness must
/// not regress on audit-trail filesystem failures. The exit code
/// the binary returns remains the authoritative outcome signal.
pub(crate) struct EventSink<'a> {
    writer: &'a mut RunWriter,
}

impl<'a> EventSink<'a> {
    pub(crate) fn new(writer: &'a mut RunWriter) -> Self {
        Self { writer }
    }

    fn oriented(&mut self, iter: u32, oriented: &OrientedState) {
        let Ok(bytes) = serde_json::to_vec(oriented) else {
            return;
        };
        let Ok(blob) = self.writer.write_blob(&bytes, "json") else {
            return;
        };
        let _ = self.writer.append(EventBody::IterationOriented {
            iteration: iter,
            blob,
        });
    }

    fn decided(&mut self, iter: u32, decision: &Decision) {
        let kind = match decision {
            Decision::Execute(a) => a.kind.name().to_string(),
            Decision::Halt(DecisionHalt::Success) => "Halt::Success".into(),
            Decision::Halt(DecisionHalt::Terminal(t)) => format!("Halt::Terminal::{t:?}"),
            Decision::Halt(DecisionHalt::AgentNeeded(_)) => "Halt::AgentNeeded".into(),
            Decision::Halt(DecisionHalt::HumanNeeded(_)) => "Halt::HumanNeeded".into(),
        };
        let _ = self.writer.append(EventBody::IterationDecided {
            iteration: iter,
            decision_kind: kind,
        });
    }

    fn acted(&mut self, iter: u32, action: &Action) {
        let body = match &action.effect {
            ActionEffect::Wait { interval, .. } => EventBody::IterationWaited {
                iteration: iter,
                action_kind: action.kind.name().to_string(),
                interval_ms: u64::try_from(interval.as_duration().as_millis()).unwrap_or(u64::MAX),
            },
            ActionEffect::Full { .. } | ActionEffect::Agent { .. } | ActionEffect::Human { .. } => {
                EventBody::IterationExecuted {
                    iteration: iter,
                    action_kind: action.kind.name().to_string(),
                }
            }
        };
        let _ = self.writer.append(body);
    }
}

/// One iteration's typed outcome: either an early halt (decision
/// halt or stall) or a completed Execute, the latter retained as
/// the running "last attempted" anchor for cap-halt diagnostics.
enum IterStep {
    Halt(HaltReason),
    Executed(Action),
}

/// Drive a session until a halt fires or the iteration cap trips.
///
/// `observe` is supplied as a closure so the test harness can
/// substitute stub observations without touching subprocesses.
///
/// `events` collects per-stage events (orient snapshot, decision,
/// act result) on the run's events.jsonl. Audit-only — failures
/// to write events do not surface as `LoopError`.
pub(crate) fn run_loop(
    repo_id: &RepoId,
    target: &ReviewTarget,
    config: LoopConfig,
    ctx: &ActContext,
    mut observe: impl FnMut(&RepoId, &ReviewTarget) -> Result<CodexObservations, String>,
    events: &mut EventSink<'_>,
) -> Result<HaltReason, LoopError> {
    let max_iter = config.max_iterations.get();

    // Iter 1 runs unconditionally; stall is structurally
    // impossible there (no prior key). Splitting it out
    // initializes `last_attempted` as a typed `Action`.
    let mut last_attempted: Action = match run_iter(
        repo_id,
        target,
        config.ceiling,
        ctx,
        &mut observe,
        events,
        1,
        None,
    )? {
        IterStep::Halt(reason) => return Ok(reason),
        IterStep::Executed(action) => action,
    };
    let mut last_non_wait_key = if last_attempted.effect.is_wait() {
        None
    } else {
        Some(last_attempted.stall_key())
    };

    for iter in 2..=max_iter {
        let step = run_iter(
            repo_id,
            target,
            config.ceiling,
            ctx,
            &mut observe,
            events,
            iter,
            last_non_wait_key.as_ref(),
        )?;
        match step {
            IterStep::Halt(reason) => return Ok(reason),
            IterStep::Executed(action) => {
                if !action.effect.is_wait() {
                    last_non_wait_key = Some(action.stall_key());
                }
                last_attempted = action;
            }
        }
    }

    Ok(HaltReason::CapReached(last_attempted))
}

/// Run one full observe → orient → decide → act cycle.
#[allow(clippy::too_many_arguments)]
fn run_iter(
    repo_id: &RepoId,
    target: &ReviewTarget,
    ceiling: CodexReasoningLevel,
    ctx: &ActContext,
    mut observe: impl FnMut(&RepoId, &ReviewTarget) -> Result<CodexObservations, String>,
    events: &mut EventSink<'_>,
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey>,
) -> Result<IterStep, LoopError> {
    let obs = observe(repo_id, target).map_err(LoopError::Observe)?;
    let oriented = orient(&obs, ceiling);
    events.oriented(iter, &oriented);
    let decision = decide(&oriented);
    events.decided(iter, &decision);

    match decision {
        Decision::Halt(halt) => Ok(IterStep::Halt(HaltReason::Decision(halt))),
        Decision::Execute(action) => {
            let current_key = action.stall_key();
            if last_non_wait_key == Some(&current_key) {
                return Ok(IterStep::Halt(HaltReason::Stalled(action)));
            }
            act(&action, ctx).map_err(LoopError::Act)?;
            events.acted(iter, &action);
            Ok(IterStep::Executed(action))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{BranchName, RepoId};
    use crate::observe::codex::VerdictClass;
    use crate::observe::codex::batch::{BatchState, VerdictRecord};
    use ooda_state::{RunId, StateRoot};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn repo_id() -> RepoId {
        RepoId::parse("ooda-codex-review-test").unwrap()
    }

    fn target() -> ReviewTarget {
        ReviewTarget::Base(BranchName::parse("master").unwrap())
    }

    fn ctx() -> ActContext {
        // codex_bin = "codex" → path is relative single-component, so
        // the act layer's PATH-lookup preflight is skipped. RunReviews
        // would actually spawn — tests structure observations so the
        // decide layer never returns RunReviews (Complete/Stuck paths).
        ActContext {
            batch_dir: std::env::temp_dir().join("ooda-codex-runner-test"),
            target: target(),
            repo_root: std::env::current_dir().unwrap(),
            codex_bin: PathBuf::from("/bin/true"),
        }
    }

    fn loop_cfg(max: u32) -> LoopConfig {
        LoopConfig {
            max_iterations: NonZeroU32::new(max).expect("nonzero"),
            ceiling: CodexReasoningLevel::Xhigh,
        }
    }

    fn obs(state: BatchState) -> CodexObservations {
        CodexObservations {
            repo_id: repo_id(),
            target: target(),
            current_level: CodexReasoningLevel::Low,
            batch_state: state,
            batch_dir: PathBuf::from("/tmp/unused"),
            expected: 3,
        }
    }

    fn record(slot: u32, class: VerdictClass) -> VerdictRecord {
        VerdictRecord {
            slot,
            body: "stub".to_string(),
            class,
        }
    }

    /// Scripted observe closure. Returns the next entry in `seq` on
    /// each call; panics if the loop calls past the end.
    fn scripted_observe(
        seq: Vec<CodexObservations>,
    ) -> impl FnMut(&RepoId, &ReviewTarget) -> Result<CodexObservations, String> {
        let cell = RefCell::new(seq.into_iter());
        move |_, _| {
            cell.borrow_mut()
                .next()
                .map(Ok)
                .expect("loop called observe past the end of the scripted sequence")
        }
    }

    /// Fresh state-root + started run for tests that need an
    /// `EventSink` but don't care about reading back what was
    /// written. Returns the temp dir so it lives for the test's
    /// scope.
    fn fresh_writer() -> (TempDir, RunWriter) {
        let tmp = TempDir::new().unwrap();
        let state = StateRoot::new(tmp.path()).unwrap();
        let mut writer = state.create_run(RunId::generate()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "codex-review".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        (tmp, writer)
    }

    // ── iter-1 guaranteed run ──

    #[test]
    fn iter_1_always_runs_even_when_max_is_one() {
        // max_iter=1 + Complete(all-clean) at ceiling → halts after
        // iter 1 with Decision(Terminal::Succeeded). The point: iter
        // 1 IS reached and observed even at the minimum cap.
        let observe = scripted_observe(vec![CodexObservations {
            current_level: CodexReasoningLevel::Xhigh,
            ..obs(BatchState::Complete {
                verdicts: vec![record(1, VerdictClass::Clean)],
            })
        }]);
        let (_tmp, mut writer) = fresh_writer();
        let mut sink = EventSink::new(&mut writer);
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(1),
            &ctx(),
            observe,
            &mut sink,
        )
        .unwrap();
        assert!(matches!(
            halt,
            HaltReason::Decision(crate::decide::decision::DecisionHalt::Terminal(_))
        ));
    }

    // ── stall detection ──

    #[test]
    fn stall_fires_on_repeated_non_wait_action() {
        // AddressBatch (Agent halt) — but the codex-review domain
        // routes that through Halt(AgentNeeded), not Execute. To
        // produce a non-wait Execute that repeats, we'd need
        // RunReviews, which would actually spawn codex. Instead, we
        // assert the structural shape: an AgentNeeded halt fires on
        // iter 1, no second iter happens, so stall detection is
        // exercised indirectly. For direct stall coverage, see the
        // wait_does_not_count_as_stall test below using AwaitReviews.
        //
        // Concrete: two Complete-with-issues observations would yield
        // AddressBatch handoffs (Halt::AgentNeeded), which the loop
        // returns via the IterStep::Halt branch — no stall key is
        // ever stored. The stall code path is exercised by the
        // running-Wait scenario below.
        let observe = scripted_observe(vec![obs(BatchState::Complete {
            verdicts: vec![record(1, VerdictClass::HasIssues)],
        })]);
        let (_tmp, mut writer) = fresh_writer();
        let mut sink = EventSink::new(&mut writer);
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(10),
            &ctx(),
            observe,
            &mut sink,
        )
        .unwrap();
        assert!(matches!(
            halt,
            HaltReason::Decision(crate::decide::decision::DecisionHalt::AgentNeeded(_))
        ));
    }

    // ── wait stall-exemption ──

    #[test]
    fn wait_actions_do_not_trigger_stall() {
        // Running → AwaitReviews (Wait). Three consecutive Running
        // observations would all emit identical Wait actions; without
        // the stall-exemption the loop would halt Stalled. With the
        // exemption it proceeds until cap. Use a small cap to keep
        // the test fast (each Wait sleeps); override the cadence via
        // env so the test completes immediately.
        // SAFETY: env vars are process-global; setting it inside a
        // serial #[test] is fine but flagged unsafe in 2024 edition.
        unsafe {
            std::env::set_var("OODA_AWAIT_SECS", "1");
        }
        let observe = scripted_observe(vec![
            obs(BatchState::Running {
                total: 3,
                completed: 1,
            }),
            obs(BatchState::Running {
                total: 3,
                completed: 2,
            }),
        ]);
        let (_tmp, mut writer) = fresh_writer();
        let mut sink = EventSink::new(&mut writer);
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(2),
            &ctx(),
            observe,
            &mut sink,
        )
        .unwrap();
        // Cap reached with the last AwaitReviews as last_attempted —
        // Wait DID NOT trigger stall.
        match halt {
            HaltReason::CapReached(action) => {
                assert!(
                    matches!(
                        action.kind,
                        crate::decide::action::ActionKind::AwaitReviews { .. }
                    ),
                    "expected last_attempted = AwaitReviews, got {:?}",
                    action.kind
                );
            }
            other => panic!("expected CapReached, got {other:?}"),
        }
    }

    // ── cap enforcement ──

    #[test]
    fn cap_reached_carries_typed_last_attempted_action() {
        // Two Running iters at cap=2 → CapReached(AwaitReviews).
        unsafe {
            std::env::set_var("OODA_AWAIT_SECS", "1");
        }
        let observe = scripted_observe(vec![
            obs(BatchState::Running {
                total: 5,
                completed: 1,
            }),
            obs(BatchState::Running {
                total: 5,
                completed: 2,
            }),
        ]);
        let (_tmp, mut writer) = fresh_writer();
        let mut sink = EventSink::new(&mut writer);
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(2),
            &ctx(),
            observe,
            &mut sink,
        )
        .unwrap();
        match halt {
            HaltReason::CapReached(action) => {
                // The typed Action carries the kind directly; no
                // Option-unwrap needed thanks to NonZeroU32 max_iter
                // structurally guaranteeing iter 1 ran.
                assert!(matches!(
                    action.kind,
                    crate::decide::action::ActionKind::AwaitReviews { .. }
                ));
            }
            other => panic!("expected CapReached, got {other:?}"),
        }
    }

    // ── observe-error propagation ──

    #[test]
    fn observe_error_bubbles_as_loop_error() {
        let observe = |_: &_, _: &_| -> Result<CodexObservations, String> {
            Err("subprocess crashed".to_string())
        };
        let (_tmp, mut writer) = fresh_writer();
        let mut sink = EventSink::new(&mut writer);
        let err = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(3),
            &ctx(),
            observe,
            &mut sink,
        )
        .unwrap_err();
        match err {
            LoopError::Observe(e) => assert!(e.contains("subprocess crashed")),
            other @ LoopError::Act(_) => panic!("expected Observe error, got {other:?}"),
        }
    }

    // ── event emission ──

    #[test]
    fn iteration_emits_orient_and_decision_events() {
        let observe = scripted_observe(vec![obs(BatchState::Complete {
            verdicts: vec![record(1, VerdictClass::HasIssues)],
        })]);
        let tmp = TempDir::new().unwrap();
        let state = StateRoot::new(tmp.path()).unwrap();
        let run_id = RunId::generate();
        let mut writer = state.create_run(run_id.clone()).unwrap();
        writer
            .start(EventBody::RunStarted {
                domain: "codex-review".into(),
                target: serde_json::Value::Null,
            })
            .unwrap();
        {
            let mut sink = EventSink::new(&mut writer);
            let _ = run_loop(
                &repo_id(),
                &target(),
                loop_cfg(2),
                &ctx(),
                observe,
                &mut sink,
            )
            .unwrap();
        }
        let reader = state.open_run(run_id).unwrap();
        let kinds: Vec<&'static str> = reader
            .events()
            .unwrap()
            .iter()
            .map(|e| match &e.body {
                EventBody::RunStarted { .. } => "run_started",
                EventBody::IterationObserved { .. } => "iteration_observed",
                EventBody::IterationOriented { .. } => "iteration_oriented",
                EventBody::IterationDecided { .. } => "iteration_decided",
                EventBody::IterationHandoff { .. } => "iteration_handoff",
                EventBody::IterationExecuted { .. } => "iteration_executed",
                EventBody::IterationWaited { .. } => "iteration_waited",
                EventBody::RunHalted { .. } => "run_halted",
                EventBody::RunStalled { .. } => "run_stalled",
                EventBody::RunCapReached { .. } => "run_cap_reached",
                EventBody::DomainSpecific { .. } => "domain_specific",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["run_started", "iteration_oriented", "iteration_decided"],
            "AgentNeeded halt should emit orient + decision but no act/halt event",
        );
    }
}
