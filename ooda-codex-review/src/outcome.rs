//! Binary boundary type — what each invocation produces.
//!
//! Re-exported from [`ooda_core`] specialised to this binary's
//! [`ActionKind`]. The generic shape, exit-code mapping, and
//! `HaltReason → Outcome` / `Decision → Outcome` conversions live
//! in the shared crate. This module adds the per-binary
//! `LoopError → Outcome` conversion.
//!
//! Domain mapping of the shared variants for the codex-review
//! domain:
//!   - `Outcome::DoneSucceeded` (exit 0) — fixed point reached at
//!     the configured ceiling reasoning level. Per-binary
//!     `render_outcome` emits `"DoneFixedPoint"` as the stderr
//!     header.
//!   - `Outcome::DoneAborted` (exit 8) — user aborted the review
//!     loop (SIGINT, `--abort`). Renderer emits `"DoneAborted"`.
//!   - `Outcome::Paused` (exit 7) — loop has nothing to drive this
//!     pass. Renderer emits `"Idle"`.

use crate::decide::action::ActionKind;
use crate::runner::LoopError;

/// Codex-review-domain `Outcome`. 1:1 variant → exit-code via
/// [`ooda_core::Outcome::exit_code`].
pub type Outcome = ooda_core::Outcome<ActionKind>;

/// `LoopError` → `BinaryError(String)`. The caller sees a
/// single-line human-triage string; the typed error is flattened
/// via [`ooda_core::Outcome::binary_error`] so the stderr-header
/// invariant ("first line is the header, nothing else follows
/// except prompt blocks for handoff variants") holds.
impl From<LoopError> for Outcome {
    fn from(err: LoopError) -> Self {
        Self::binary_error(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{
        Action, ActionKind, Automation, ReasoningLevel, TargetEffect, Urgency,
    };
    use crate::decide::decision::{Decision, DecisionHalt, HaltReason, Terminal};
    use crate::ids::BlockerKey;

    fn dummy_action() -> Action {
        Action {
            kind: ActionKind::RunReviews {
                level: ReasoningLevel::Low,
                n: 3,
            },
            automation: Automation::Full,
            target_effect: TargetEffect::Advances,
            urgency: Urgency::Critical,
            description: "test".into(),
            blocker: BlockerKey::tag("not-started"),
        }
    }

    #[test]
    fn outcome_maps_to_matching_exit_code_variant() {
        use ooda_core::ExitCode;
        assert_eq!(Outcome::DoneSucceeded.exit_code(), ExitCode::DoneSucceeded);
        assert_eq!(Outcome::Paused.exit_code(), ExitCode::Paused);
        assert_eq!(
            Outcome::WouldAdvance(dummy_action()).exit_code(),
            ExitCode::WouldAdvance
        );
        assert_eq!(
            Outcome::HandoffHuman(dummy_action()).exit_code(),
            ExitCode::HandoffHuman
        );
        assert_eq!(
            Outcome::HandoffAgent(dummy_action()).exit_code(),
            ExitCode::HandoffAgent
        );
        assert_eq!(Outcome::DoneAborted.exit_code(), ExitCode::DoneAborted);
        assert_eq!(
            Outcome::StuckRepeated(dummy_action()).exit_code(),
            ExitCode::StuckRepeated
        );
        assert_eq!(
            Outcome::StuckCapReached(dummy_action()).exit_code(),
            ExitCode::StuckCapReached
        );
        assert_eq!(
            Outcome::UsageError("bad".into()).exit_code(),
            ExitCode::UsageError
        );
        assert_eq!(
            Outcome::BinaryError("oops".into()).exit_code(),
            ExitCode::BinaryError
        );
    }

    #[test]
    fn halt_reason_maps_terminals_to_done_variants() {
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Succeeded
            ))),
            Outcome::DoneSucceeded
        ));
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Aborted
            ))),
            Outcome::DoneAborted
        ));
    }

    #[test]
    fn halt_reason_maps_success_to_paused() {
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::Success)),
            Outcome::Paused
        ));
    }

    #[test]
    fn halt_reason_maps_handoffs() {
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::AgentNeeded(
                dummy_action()
            ))),
            Outcome::HandoffAgent(_)
        ));
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::HumanNeeded(
                dummy_action()
            ))),
            Outcome::HandoffHuman(_)
        ));
    }

    #[test]
    fn halt_reason_maps_stalled_and_cap() {
        assert!(matches!(
            Outcome::from(HaltReason::Stalled(dummy_action())),
            Outcome::StuckRepeated(_)
        ));
        assert!(matches!(
            Outcome::from(HaltReason::CapReached(dummy_action())),
            Outcome::StuckCapReached(_)
        ));
    }

    #[test]
    fn decision_execute_maps_to_would_advance() {
        assert!(matches!(
            Outcome::from(Decision::Execute(dummy_action())),
            Outcome::WouldAdvance(_)
        ));
    }

    #[test]
    fn decision_halts_pass_through_inspect() {
        assert!(matches!(
            Outcome::from(Decision::Halt(DecisionHalt::Success)),
            Outcome::Paused
        ));
        assert!(matches!(
            Outcome::from(Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded))),
            Outcome::DoneSucceeded
        ));
    }
}
