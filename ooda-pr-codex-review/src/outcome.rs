//! Binary boundary type — what each invocation produces.
//!
//! The generic [`Outcome`] shape and its exit-code projection live in
//! the shared crate. This module fixes the type parameter to the
//! PR-domain action and supplies the per-binary error-to-outcome
//! conversion — necessarily local because each binary's error sum
//! has its own variant set.

use crate::decide::action::ActionKind;
use crate::runner::LoopError;

/// PR-domain `Outcome`. Variant → exit-code is bijective via
/// [`ooda_core::Outcome::exit_code`].
pub(crate) type Outcome = ooda_core::Outcome<ActionKind>;

/// Project a `LoopError` into the binary-error outcome variant.
///
/// Invariant: the stderr header surface is single-line. Typed
/// errors are flattened to one human-triage string at this boundary
/// so the header invariant holds for every binary-error path.
impl From<LoopError> for Outcome {
    fn from(err: LoopError) -> Self {
        Self::binary_error(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::decide::decision::{Decision, DecisionHalt, HaltReason, Terminal};
    use crate::ids::BlockerKey;

    fn dummy_action() -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Full { log: "test".into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    fn dummy_handoff() -> ooda_core::HandoffAction<ActionKind> {
        ooda_core::HandoffAction {
            kind: ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::from_static("not-approved"),
        }
    }

    #[test]
    fn outcome_maps_to_matching_exit_code_variant() {
        use ooda_core::ExitCode;
        assert_eq!(Outcome::DoneSucceeded.exit_code(), ExitCode::DoneSucceeded);
        assert_eq!(Outcome::Paused.exit_code(), ExitCode::Paused);
        assert_eq!(
            Outcome::WouldAdvance(Box::new(dummy_action())).exit_code(),
            ExitCode::WouldAdvance
        );
        assert_eq!(
            Outcome::HandoffHuman(Box::new(dummy_handoff())).exit_code(),
            ExitCode::HandoffHuman
        );
        assert_eq!(
            Outcome::HandoffAgent(Box::new(dummy_handoff())).exit_code(),
            ExitCode::HandoffAgent
        );
        assert_eq!(Outcome::DoneAborted.exit_code(), ExitCode::DoneAborted);
        assert_eq!(
            Outcome::StuckRepeated(Box::new(dummy_action())).exit_code(),
            ExitCode::StuckRepeated
        );
        assert_eq!(
            Outcome::StuckCapReached(Box::new(dummy_action())).exit_code(),
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
                dummy_handoff()
            ))),
            Outcome::HandoffAgent(_)
        ));
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::HumanNeeded(
                dummy_handoff()
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
