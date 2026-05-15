//! Binary boundary type — what each invocation produces.
//!
//! Internal types (`Decision`, `HaltReason`, plus per-binary loop
//! errors) split halt-vs-execute and decide-level vs loop-level
//! concerns. At the binary boundary those splits collapse: callers
//! want **one** variant per invocation with **one** exit code.
//!
//! `Outcome<K>` is that boundary type. The 1:1 variant → exit-code
//! mapping is the contract; wrappers dispatch on `$?` alone.
//!
//! Construction is via `From` impls — `HaltReason<K> → Outcome<K>`
//! for loop mode, `Decision<K> → Outcome<K>` for inspect mode.
//! Per-binary `LoopError` types convert via the
//! [`Outcome::binary_error`] constructor — they're not uniform
//! enough across binaries to support a blanket `From` here.
//! Argument-parse and main-routine variants (`UsageError`,
//! `Paused`, `DoneClosed`) are constructed directly.

use crate::action::Action;
use crate::decision::{Decision, DecisionHalt, HaltReason, Terminal};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub enum Outcome<K> {
    /// Target reached its terminal success state (PR merged, ladder
    /// satisfied, etc.).
    DoneMerged,
    /// Same `(kind, blocker)` action repeated on consecutive
    /// non-`Wait` iterations. Carries the repeated action.
    StuckRepeated(Action<K>),
    /// Iteration cap hit. Carries the last attempted action — the
    /// natural triage anchor (`<ActionKind>:<BlockerKey>` shows
    /// what was running when the cap fired).
    StuckCapReached(Action<K>),
    /// Decide selected an action requiring a human. Carries the
    /// action; `act` did not run.
    HandoffHuman(Action<K>),
    /// Inspect-only. Decide selected an `Execute(action)`; the loop
    /// would have run it, inspect halts before acting.
    WouldAdvance(Action<K>),
    /// Decide selected an action requiring an agent. Carries the
    /// action; `act` did not run.
    HandoffAgent(Action<K>),
    /// Caught external failure (subprocess, network, IO). String is
    /// for human triage; no embedded newlines.
    BinaryError(String),
    /// Decide selected no candidate action — target is open with
    /// no advancing work this pass. May re-invoke later.
    Paused,
    /// Target closed without reaching success (PR closed, ladder
    /// aborted). Terminal but non-success.
    DoneClosed,
    /// CLI parse / validation failure. String is the diagnostic;
    /// no embedded newlines.
    UsageError(String),
}

impl<K> Outcome<K> {
    /// 1:1 variant → exit-code. The contract.
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

    /// Per-binary loop errors funnel through here. The argument is
    /// flattened to a single line so the documented invariant
    /// ("`BinaryError: <msg>` header is one line") holds.
    pub fn binary_error(msg: impl Into<String>) -> Self {
        Self::BinaryError(flatten_one_line(msg.into()))
    }
}

/// Loop mode: collapse the runner's `HaltReason` taxonomy into the
/// boundary `Outcome`.
impl<K> From<HaltReason<K>> for Outcome<K> {
    fn from(reason: HaltReason<K>) -> Self {
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
impl<K> From<Decision<K>> for Outcome<K> {
    fn from(decision: Decision<K>) -> Self {
        match decision {
            Decision::Execute(action) => Self::WouldAdvance(action),
            Decision::Halt(halt) => decision_halt_to_outcome(halt),
        }
    }
}

fn decision_halt_to_outcome<K>(halt: DecisionHalt<K>) -> Outcome<K> {
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
    fn exit_codes_match_spec() {
        assert_eq!(Outcome::<K>::DoneMerged.exit_code(), 0);
        assert_eq!(Outcome::StuckRepeated(dummy()).exit_code(), 1);
        assert_eq!(Outcome::StuckCapReached(dummy()).exit_code(), 2);
        assert_eq!(Outcome::HandoffHuman(dummy()).exit_code(), 3);
        assert_eq!(Outcome::WouldAdvance(dummy()).exit_code(), 4);
        assert_eq!(Outcome::HandoffAgent(dummy()).exit_code(), 5);
        assert_eq!(Outcome::<K>::BinaryError("oops".into()).exit_code(), 6);
        assert_eq!(Outcome::<K>::Paused.exit_code(), 7);
        assert_eq!(Outcome::<K>::DoneClosed.exit_code(), 8);
        assert_eq!(Outcome::<K>::UsageError("bad".into()).exit_code(), 64);
    }

    #[test]
    fn halt_reason_maps_terminals_to_done_variants() {
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Merged
            ))),
            Outcome::DoneMerged
        ));
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Closed
            ))),
            Outcome::DoneClosed
        ));
    }

    #[test]
    fn halt_reason_maps_success_to_paused() {
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::Success)),
            Outcome::Paused
        ));
    }

    #[test]
    fn halt_reason_maps_handoffs() {
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::AgentNeeded(dummy()))),
            Outcome::HandoffAgent(_)
        ));
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::HumanNeeded(dummy()))),
            Outcome::HandoffHuman(_)
        ));
    }

    #[test]
    fn halt_reason_maps_stalled_and_cap() {
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Stalled(dummy())),
            Outcome::StuckRepeated(_)
        ));
        assert!(matches!(
            Outcome::<K>::from(HaltReason::CapReached(dummy())),
            Outcome::StuckCapReached(_)
        ));
    }

    #[test]
    fn decision_execute_maps_to_would_advance() {
        assert!(matches!(
            Outcome::<K>::from(Decision::Execute(dummy())),
            Outcome::WouldAdvance(_)
        ));
    }

    #[test]
    fn decision_halts_pass_through_inspect() {
        assert!(matches!(
            Outcome::<K>::from(Decision::Halt(DecisionHalt::Success)),
            Outcome::Paused
        ));
        assert!(matches!(
            Outcome::<K>::from(Decision::Halt(DecisionHalt::Terminal(Terminal::Merged))),
            Outcome::DoneMerged
        ));
    }

    #[test]
    fn binary_error_strips_newlines() {
        let o: Outcome<K> = Outcome::binary_error("line one\nline two\nline three");
        match o {
            Outcome::BinaryError(s) => {
                assert_eq!(s, "line one line two line three");
                assert!(!s.contains('\n'));
            }
            _ => panic!("expected BinaryError"),
        }
    }

    #[test]
    fn binary_error_preserves_single_line() {
        let o: Outcome<K> = Outcome::binary_error("single line error");
        match o {
            Outcome::BinaryError(s) => assert_eq!(s, "single line error"),
            _ => panic!("expected BinaryError"),
        }
    }
}
