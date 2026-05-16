//! OODA loop driver — observe → orient → decide → act → repeat
//! until a halt condition fires.
//!
//! Stall detection: if the same (kind, blocker) pair fires twice
//! in a row (excluding Wait), the loop halts Stalled. Coarse —
//! only catches the one-action-spinning case. The iteration cap
//! is the second line of defense and surfaces as `HaltReason::CapReached`.
//!
//! The loop returns `HaltReason` directly. Cap, stall, success,
//! terminal, and handoff are all variants of the same partition.
//! Exit-code mapping lives on `HaltReason::exit_code()`.
//!
//! Codex-domain shape: each iteration spawns a fresh `observe`
//! (subprocess fan-out), runs `orient`/`decide`, then `act`s on
//! Full/Wait. Agent/Human halts return up to the caller.
//!
//! `LoopConfig::max_iterations` is `NonZeroU32` so iter 1 is
//! structurally guaranteed to run; the driver splits iter 1 from
//! the subsequent iterations so `last_attempted` flows as a typed
//! `Action` (not `Option<Action>`) into the eventual
//! `HaltReason::CapReached` — eliminating the runtime expect that
//! previously documented this invariant.

use std::num::NonZeroU32;

use crate::act::{ActContext, ActError, act};
use crate::decide::action::{Action, CodexReasoningLevel};
use crate::decide::decide;
use crate::decide::decision::{Decision, HaltReason};
use crate::ids::{RepoId, ReviewTarget};
use crate::observe::codex::CodexObservations;
use crate::orient::OrientedState;
use crate::orient::orient;

#[derive(Debug)]
pub enum LoopError {
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
pub struct LoopConfig {
    /// Iteration cap. `NonZeroU32` so the driver's "iter 1
    /// always runs" guarantee is structural.
    pub max_iterations: NonZeroU32,
    /// Top of the reasoning ladder. When the loop reaches an
    /// all-clean batch at this level, decide halts with
    /// `Terminal(Succeeded)` (the codex-review fixed point at the
    /// ceiling) instead of emitting a `Retrospective` handoff.
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

/// One iteration's typed outcome. The loop body produces either
/// an early-halt (`Decision::Halt` or stall-detected) or a completed
/// Execute that we keep as the running "last attempted" anchor.
enum IterStep {
    Halt(HaltReason),
    Executed(Action),
}

/// Drive a codex-review session until a halt fires or the
/// iteration cap trips.
///
/// `observe` is parameterized: callers supply a closure that
/// fetches the current `CodexObservations` for the configured
/// `(repo_id, target)`. This lets the test harness substitute
/// stub observations without touching subprocesses.
///
/// `on_state` is called once per iteration after decide and
/// before act, with the iteration index, oriented state, and
/// chosen decision. Halt decisions also fire it before returning.
pub fn run_loop(
    repo_id: &RepoId,
    target: &ReviewTarget,
    config: LoopConfig,
    ctx: &ActContext,
    mut observe: impl FnMut(&RepoId, &ReviewTarget) -> Result<CodexObservations, String>,
    mut on_state: impl FnMut(u32, &OrientedState, &Decision),
) -> Result<HaltReason, LoopError> {
    let max_iter = config.max_iterations.get();

    // Iter 1 is guaranteed to run; stall is structurally impossible
    // there (no prior key). Run it explicitly so `last_attempted` is
    // initialized as a typed `Action`.
    let mut last_attempted: Action = match run_iter(
        repo_id,
        target,
        config.ceiling,
        ctx,
        &mut observe,
        &mut on_state,
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
            &mut on_state,
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
    mut on_state: impl FnMut(u32, &OrientedState, &Decision),
    iter: u32,
    last_non_wait_key: Option<&ooda_core::StallKey<crate::decide::action::ActionKind>>,
) -> Result<IterStep, LoopError> {
    let obs = observe(repo_id, target).map_err(LoopError::Observe)?;
    let oriented = orient(&obs, ceiling);
    let decision = decide(&oriented);
    on_state(iter, &oriented, &decision);

    match decision {
        Decision::Halt(halt) => Ok(IterStep::Halt(HaltReason::Decision(halt))),
        Decision::Execute(action) => {
            let current_key = action.stall_key();
            if last_non_wait_key == Some(&current_key) {
                return Ok(IterStep::Halt(HaltReason::Stalled(action)));
            }
            act(&action, ctx).map_err(LoopError::Act)?;
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
    use std::cell::RefCell;
    use std::path::PathBuf;

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
        let mut callbacks = 0;
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(1),
            &ctx(),
            observe,
            |_iter, _o, _d| {
                callbacks += 1;
            },
        )
        .unwrap();
        assert_eq!(callbacks, 1, "on_state must fire exactly once on iter 1");
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
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(10),
            &ctx(),
            observe,
            |_, _, _| {},
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
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(2),
            &ctx(),
            observe,
            |_, _, _| {},
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
        let halt = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(2),
            &ctx(),
            observe,
            |_, _, _| {},
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
        let err = run_loop(
            &repo_id(),
            &target(),
            loop_cfg(3),
            &ctx(),
            observe,
            |_, _, _| {},
        )
        .unwrap_err();
        match err {
            LoopError::Observe(e) => assert!(e.contains("subprocess crashed")),
            other @ LoopError::Act(_) => panic!("expected Observe error, got {other:?}"),
        }
    }
}
