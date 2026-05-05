//! Binary boundary type — what each invocation produces.
//!
//! Internal types (`Decision`, `HaltReason`, `LoopError`) split
//! halt-vs-execute and decide-level vs loop-level concerns. At the
//! binary boundary those splits collapse: callers want **one**
//! variant per invocation with **one** exit code.
//!
//! `Outcome` is that boundary type. 1:1 variant→exit-code mapping
//! is the contract; callers dispatch on `$?` alone (see `SKILL.md`).
//!
//! Construction is via `From` impls — `HaltReason → Outcome` for
//! loop mode, `Decision → Outcome` for inspect mode, `LoopError
//! → Outcome` for caught external failures. Argument-parse failures
//! and inspect/loop main routines construct the `UsageError` and
//! variant cases directly; this module owns the type and its
//! exit-code mapping.

use crate::decide::action::Action;
use crate::decide::decision::{Decision, DecisionHalt, HaltReason, Terminal};
use crate::runner::LoopError;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub enum Outcome {
    /// PR merged. Terminal success.
    DoneMerged,
    /// Same `(kind, blocker)` action repeated on consecutive
    /// non-`Wait` iterations. Carries the repeated action.
    StuckRepeated(Action),
    /// Iteration cap hit. Carries the last attempted action
    /// (Wait or non-Wait) — the natural triage anchor
    /// (`<ActionKind>:<BlockerKey>` shows what was running when
    /// the cap fired). The runner constructs this only when the
    /// cap fires after at least one Execute, which is guaranteed
    /// by `--max-iter ≥ 1` parser validation plus the loop's
    /// halt-on-any-Decision::Halt early return.
    StuckCapReached(Action),
    /// Decide selected an action requiring a human. Carries the
    /// action; `act` did not run.
    HandoffHuman(Action),
    /// Inspect-only. Decide selected an `Execute(action)` (i.e.
    /// `Full` or `Wait` automation); the loop would have run it,
    /// inspect halts before acting. The action's `automation`
    /// field tells the caller what `act` would do.
    WouldAdvance(Action),
    /// Decide selected an action requiring an agent. Carries the
    /// action; `act` did not run.
    HandoffAgent(Action),
    /// Caught external failure (gh subprocess, network, IO).
    /// String is for human triage; no embedded newlines.
    BinaryError(String),
    /// Decide selected no candidate action — PR is open with no
    /// advancing work this pass. May re-invoke later.
    Paused,
    /// PR closed without merge. Terminal but non-success.
    DoneClosed,
    /// CLI parse / validation failure. String is the diagnostic;
    /// no embedded newlines.
    UsageError(String),
}

impl Outcome {
    /// 1:1 variant→exit-code. The contract. See `SKILL.md`.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::DoneMerged => 0,
            Self::StuckRepeated(_) => 1,
            Self::StuckCapReached(_) => 2,
            Self::HandoffHuman(_) => 3,
            Self::WouldAdvance(_) => 4,
            Self::HandoffAgent(_) => 5,
            Self::BinaryError(_) => 6,
            Self::Paused => 7,
            Self::DoneClosed => 8,
            Self::UsageError(_) => 64,
        }
    }
}

/// Loop mode: collapse the runner's `HaltReason` taxonomy into the
/// boundary `Outcome`.
impl From<HaltReason> for Outcome {
    fn from(reason: HaltReason) -> Self {
        match reason {
            HaltReason::Decision(halt) => decision_halt_to_outcome(halt),
            HaltReason::Stalled(action) => Self::StuckRepeated(action),
            HaltReason::CapReached(action) => Self::StuckCapReached(action),
        }
    }
}

/// Inspect mode: collapse a single decide pass into the boundary
/// `Outcome`. `Execute(action)` becomes `WouldAdvance(action)`
/// because inspect halts before `act`. Halts pass through via the
/// shared `decision_halt_to_outcome`.
impl From<Decision> for Outcome {
    fn from(decision: Decision) -> Self {
        match decision {
            Decision::Execute(action) => Self::WouldAdvance(action),
            Decision::Halt(halt) => decision_halt_to_outcome(halt),
        }
    }
}

/// `LoopError` → `BinaryError(String)`. The caller sees a
/// single-line human-triage string; the typed error is flattened
/// here. Newlines in the underlying error are replaced with a
/// space so the stderr-header invariant ("first line is the
/// header, nothing else follows except prompt blocks for handoff
/// variants") holds.
impl From<LoopError> for Outcome {
    fn from(err: LoopError) -> Self {
        Self::BinaryError(flatten_one_line(err.to_string()))
    }
}

fn decision_halt_to_outcome(halt: DecisionHalt) -> Outcome {
    match halt {
        DecisionHalt::Success => Outcome::Paused,
        DecisionHalt::Terminal(Terminal::Merged) => Outcome::DoneMerged,
        DecisionHalt::Terminal(Terminal::Closed) => Outcome::DoneClosed,
        DecisionHalt::AgentNeeded(action) => Outcome::HandoffAgent(action),
        DecisionHalt::HumanNeeded(action) => Outcome::HandoffHuman(action),
    }
}

/// Strip newlines from an error string for the `BinaryError`
/// payload. Preserves the documented invariant that the
/// `BinaryError: <msg>` header is one line.
fn flatten_one_line(s: String) -> String {
    if s.contains('\n') {
        s.replace('\n', " ")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{Action, ActionKind, Automation, TargetEffect, Urgency};
    use crate::ids::BlockerKey;

    fn dummy_action() -> Action {
        Action {
            kind: ActionKind::Rebase,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: "test".into(),
            blocker: BlockerKey::tag("rebase-needed"),
        }
    }

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(Outcome::DoneMerged.exit_code(), 0);
        assert_eq!(Outcome::StuckRepeated(dummy_action()).exit_code(), 1);
        assert_eq!(Outcome::StuckCapReached(dummy_action()).exit_code(), 2);
        assert_eq!(Outcome::HandoffHuman(dummy_action()).exit_code(), 3);
        assert_eq!(Outcome::WouldAdvance(dummy_action()).exit_code(), 4);
        assert_eq!(Outcome::HandoffAgent(dummy_action()).exit_code(), 5);
        assert_eq!(Outcome::BinaryError("oops".into()).exit_code(), 6);
        assert_eq!(Outcome::Paused.exit_code(), 7);
        assert_eq!(Outcome::DoneClosed.exit_code(), 8);
        assert_eq!(Outcome::UsageError("bad".into()).exit_code(), 64);
    }

    #[test]
    fn halt_reason_maps_terminals_to_done_variants() {
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Merged
            ))),
            Outcome::DoneMerged
        ));
        assert!(matches!(
            Outcome::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Closed
            ))),
            Outcome::DoneClosed
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
            Outcome::from(Decision::Halt(DecisionHalt::Terminal(Terminal::Merged))),
            Outcome::DoneMerged
        ));
    }

    #[test]
    fn binary_error_strips_newlines() {
        let multi_line = "line one\nline two\nline three".to_string();
        let flat = flatten_one_line(multi_line);
        assert_eq!(flat, "line one line two line three");
        assert!(!flat.contains('\n'));
    }

    #[test]
    fn binary_error_preserves_single_line() {
        let single = "single line error".to_string();
        assert_eq!(flatten_one_line(single.clone()), single);
    }
}
