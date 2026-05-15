//! Three-layered halt taxonomy.
//!
//! `decide()` returns [`Decision<K>`]; `run_loop` returns
//! [`HaltReason<K>`]. Splitting them gives the compiler proof that
//! render code observing only decide-level halts need not handle
//! `Stalled` or `CapReached`. Unifying would force dead match arms.
//!
//! Exit-code mapping is documented per-type ([`Decision::exit_code`],
//! [`HaltReason::exit_code`]) so the taxonomy and its IPC encoding
//! share one source of truth.

use crate::action::Action;
use crate::exit_code::ExitCode;
use serde::Serialize;

/// What the loop should do next. Returned by `decide()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Decision<K> {
    /// Dispatch this action and re-iterate. Decide picked it from
    /// the candidate set; runtime semantics depend on `automation`.
    Execute(Action<K>),
    /// Stop iterating. Surface the reason to the caller.
    Halt(DecisionHalt<K>),
}

impl<K> Decision<K> {
    /// Documented exit-code mapping. `Execute` maps to
    /// [`ExitCode::WouldAdvance`]: the full loop would auto-run
    /// the action, but a single-pass probe (`inspect`) does not —
    /// wrappers gating on success must see a non-zero exit so a
    /// still-advancing target doesn't look green. An inspect pass
    /// that would have executed produces the same `$?` as a
    /// `WouldAdvance` halt.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Execute(_) => ExitCode::WouldAdvance,
            Self::Halt(halt) => halt.exit_code(),
        }
    }
}

/// Why `decide()` returned a halt. Pure function of orient output;
/// no loop-level state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DecisionHalt<K> {
    /// No actions to dispatch, no blockers — target reached.
    Success,
    /// Target is in a terminal lifecycle state.
    Terminal(Terminal),
    /// Top candidate requires an agent to execute. Outer driver
    /// runs the agent and re-invokes.
    AgentNeeded(Action<K>),
    /// Top candidate requires a human. Outer driver surfaces and
    /// waits.
    HumanNeeded(Action<K>),
}

impl<K> DecisionHalt<K> {
    /// Decide-level exit codes track [`crate::Outcome`]:
    /// `Success` and `Terminal` halt the loop with
    /// [`ExitCode::DoneSucceeded`] (Paused is not produced at the
    /// decide layer); `HumanNeeded` maps to [`ExitCode::HandoffHuman`];
    /// `AgentNeeded` maps to [`ExitCode::HandoffAgent`].
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Success | Self::Terminal(_) => ExitCode::DoneSucceeded,
            Self::HumanNeeded(_) => ExitCode::HandoffHuman,
            Self::AgentNeeded(_) => ExitCode::HandoffAgent,
        }
    }

    /// Stable, finite, single-token rendering for the per-iteration
    /// halt log line. Distinct from `Debug` (which would dump full
    /// Action payloads and break the one-line invariant).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::Terminal(Terminal::Succeeded) => "Terminal(Succeeded)",
            Self::Terminal(Terminal::Aborted) => "Terminal(Aborted)",
            Self::AgentNeeded(_) => "AgentNeeded",
            Self::HumanNeeded(_) => "HumanNeeded",
        }
    }
}

/// Why `run_loop` stopped. Superset of [`DecisionHalt`] with the
/// two loop-level halt classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum HaltReason<K> {
    /// `decide()` produced a halt this iteration. Carries the
    /// underlying decide-level reason.
    Decision(DecisionHalt<K>),
    /// Same `(kind, blocker)` action fired twice in a row without
    /// observable state change. Carries the repeated action so
    /// callers can triage without re-deriving from logs.
    Stalled(Action<K>),
    /// Iteration cap hit without halting. Carries the last
    /// attempted action (Wait or non-Wait).
    CapReached(Action<K>),
}

impl<K> HaltReason<K> {
    /// Exit-code mapping. [`ExitCode::UsageError`] and
    /// [`ExitCode::BinaryError`] live outside this enum: they
    /// describe CLI *parse* failure and caught *external* failure
    /// (subprocess, IO, etc.), neither of which is a halt. Both
    /// are encoded on [`crate::Outcome`] directly.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Decision(halt) => halt.exit_code(),
            Self::Stalled(_) => ExitCode::StuckRepeated,
            Self::CapReached(_) => ExitCode::StuckCapReached,
        }
    }
}

/// Terminal lifecycle states. Domain-specific instances:
/// `Succeeded` covers PR-merged and codex-ladder-fixed-point;
/// `Aborted` covers PR-closed-without-merge and ladder-abandoned.
/// Stable, neutral verbs so the same enum serves every binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Terminal {
    Succeeded,
    Aborted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, Automation, TargetEffect, Urgency};
    use crate::blocker::BlockerKey;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    struct K;

    fn dummy() -> Action<K> {
        Action {
            kind: K,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: "x".into(),
            blocker: BlockerKey::tag("t"),
        }
    }

    #[test]
    fn decision_execute_maps_to_would_advance() {
        assert_eq!(
            Decision::Execute(dummy()).exit_code(),
            ExitCode::WouldAdvance
        );
    }

    #[test]
    fn decision_halt_success_maps_to_done_succeeded() {
        let d: Decision<K> = Decision::Halt(DecisionHalt::Success);
        assert_eq!(d.exit_code(), ExitCode::DoneSucceeded);
    }

    #[test]
    fn decision_halt_terminal_maps_to_done_succeeded() {
        let d: Decision<K> = Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded));
        assert_eq!(d.exit_code(), ExitCode::DoneSucceeded);
        let d: Decision<K> = Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted));
        assert_eq!(d.exit_code(), ExitCode::DoneSucceeded);
    }

    #[test]
    fn decision_halt_handoffs_have_distinct_codes() {
        assert_eq!(
            DecisionHalt::<K>::Success.exit_code(),
            ExitCode::DoneSucceeded
        );
        assert_eq!(
            DecisionHalt::HumanNeeded(dummy()).exit_code(),
            ExitCode::HandoffHuman
        );
        assert_eq!(
            DecisionHalt::AgentNeeded(dummy()).exit_code(),
            ExitCode::HandoffAgent
        );
    }

    #[test]
    fn halt_reason_layers_exit_codes() {
        assert_eq!(
            HaltReason::Decision(DecisionHalt::<K>::Success).exit_code(),
            ExitCode::DoneSucceeded
        );
        assert_eq!(
            HaltReason::Stalled(dummy()).exit_code(),
            ExitCode::StuckRepeated
        );
        assert_eq!(
            HaltReason::CapReached(dummy()).exit_code(),
            ExitCode::StuckCapReached
        );
        assert_eq!(
            HaltReason::Decision(DecisionHalt::HumanNeeded(dummy())).exit_code(),
            ExitCode::HandoffHuman
        );
        assert_eq!(
            HaltReason::Decision(DecisionHalt::AgentNeeded(dummy())).exit_code(),
            ExitCode::HandoffAgent
        );
    }

    #[test]
    fn decision_halt_name_is_payload_free() {
        assert_eq!(DecisionHalt::<K>::Success.name(), "Success");
        assert_eq!(
            DecisionHalt::<K>::Terminal(Terminal::Succeeded).name(),
            "Terminal(Succeeded)"
        );
        assert_eq!(
            DecisionHalt::<K>::Terminal(Terminal::Aborted).name(),
            "Terminal(Aborted)"
        );
        assert_eq!(DecisionHalt::AgentNeeded(dummy()).name(), "AgentNeeded");
        assert_eq!(DecisionHalt::HumanNeeded(dummy()).name(), "HumanNeeded");
    }
}
