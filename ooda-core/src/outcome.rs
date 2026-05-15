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

use crate::action::{Action, HandoffAction};
use crate::decision::{Decision, DecisionHalt, HaltReason, Terminal};
use crate::exit_code::ExitCode;
use crate::single_line_string::SingleLineString;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub enum Outcome<K> {
    /// Target reached its terminal success state. Domain-specific
    /// instances: PR merged, codex-review ladder satisfied, etc.
    /// Per-binary `render_outcome` emits the domain-specific stderr
    /// header (e.g. `DoneMerged`, `DoneFixedPoint`); this variant
    /// name is internal.
    DoneSucceeded,
    /// Same `(kind, blocker)` action repeated on consecutive
    /// non-`Wait` iterations. Carries the repeated action.
    ///
    /// `Action` is boxed because `Outcome` is used as the `Err`
    /// type in `Result<_, Outcome>` chains; keeping the variant
    /// small avoids the `clippy::result_large_err` lint and keeps
    /// the success path cheap.
    StuckRepeated(Box<Action<K>>),
    /// Iteration cap hit. Carries the last attempted action — the
    /// natural triage anchor (`<ActionKind>:<BlockerKey>` shows
    /// what was running when the cap fired). Boxed; see
    /// [`Self::StuckRepeated`].
    StuckCapReached(Box<Action<K>>),
    /// Decide selected an action requiring a human. Carries the
    /// handoff projection; `act` did not run. Boxed; see
    /// [`Self::StuckRepeated`].
    HandoffHuman(Box<HandoffAction<K>>),
    /// Inspect-only. Decide selected an `Execute(action)`; the loop
    /// would have run it, inspect halts before acting. Boxed; see
    /// [`Self::StuckRepeated`].
    WouldAdvance(Box<Action<K>>),
    /// Decide selected an action requiring an agent. Carries the
    /// handoff projection; `act` did not run. Boxed; see
    /// [`Self::StuckRepeated`].
    HandoffAgent(Box<HandoffAction<K>>),
    /// Caught external failure (subprocess, network, IO). The
    /// [`SingleLineString`] payload is for human triage; the
    /// no-newlines invariant is enforced by the type so the
    /// "stderr header is one line" contract holds by construction.
    BinaryError(SingleLineString),
    /// Decide selected no candidate action — target is open with
    /// no advancing work this pass. May re-invoke later.
    Paused,
    /// Target reached a terminal non-success state (PR closed, ladder
    /// abandoned). Per-binary `render_outcome` emits the
    /// domain-specific stderr header (e.g. `DoneClosed`,
    /// `DoneAborted`).
    DoneAborted,
    /// CLI parse / validation failure. The [`SingleLineString`]
    /// payload is the diagnostic; the no-newlines invariant is
    /// enforced by the type.
    UsageError(SingleLineString),
}

impl<K> Outcome<K> {
    /// 1:1 variant → exit-code. The contract.
    ///
    /// Returns an [`ExitCode`] rather than a raw `u8` so the
    /// numeric values live in exactly one place
    /// (`exit_code.rs`). Convert via `u8::from(_)` /
    /// `i32::from(_)` when handing to `std::process::exit`.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::DoneSucceeded => ExitCode::DoneSucceeded,
            Self::Paused => ExitCode::Paused,
            Self::WouldAdvance(_) => ExitCode::WouldAdvance,
            Self::HandoffHuman(_) => ExitCode::HandoffHuman,
            Self::HandoffAgent(_) => ExitCode::HandoffAgent,
            Self::DoneAborted => ExitCode::DoneAborted,
            Self::StuckRepeated(_) => ExitCode::StuckRepeated,
            Self::StuckCapReached(_) => ExitCode::StuckCapReached,
            Self::UsageError(_) => ExitCode::UsageError,
            Self::BinaryError(_) => ExitCode::BinaryError,
        }
    }

    /// Per-binary loop errors funnel through here. The
    /// [`SingleLineString`] type enforces the
    /// "`BinaryError: <msg>` header is one line" invariant by
    /// construction.
    pub fn binary_error(msg: impl Into<SingleLineString>) -> Self {
        Self::BinaryError(msg.into())
    }

    /// CLI parse / validation failure constructor. Same
    /// single-line invariant as [`Self::binary_error`].
    pub fn usage_error(msg: impl Into<SingleLineString>) -> Self {
        Self::UsageError(msg.into())
    }
}

/// Loop mode: collapse the runner's `HaltReason` taxonomy into the
/// boundary `Outcome`.
impl<K> From<HaltReason<K>> for Outcome<K> {
    fn from(reason: HaltReason<K>) -> Self {
        match reason {
            HaltReason::Decision(halt) => decision_halt_to_outcome(halt),
            HaltReason::Stalled(action) => Self::StuckRepeated(Box::new(action)),
            HaltReason::CapReached(action) => Self::StuckCapReached(Box::new(action)),
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
            Decision::Execute(action) => Self::WouldAdvance(Box::new(action)),
            Decision::Halt(halt) => decision_halt_to_outcome(halt),
        }
    }
}

fn decision_halt_to_outcome<K>(halt: DecisionHalt<K>) -> Outcome<K> {
    match halt {
        DecisionHalt::Success => Outcome::Paused,
        DecisionHalt::Terminal(Terminal::Succeeded) => Outcome::DoneSucceeded,
        DecisionHalt::Terminal(Terminal::Aborted) => Outcome::DoneAborted,
        DecisionHalt::AgentNeeded(action) => Outcome::HandoffAgent(Box::new(action)),
        DecisionHalt::HumanNeeded(action) => Outcome::HandoffHuman(Box::new(action)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, ActionEffect, TargetEffect, Urgency};
    use crate::blocker::BlockerKey;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    struct K;

    fn dummy() -> Action<K> {
        Action {
            kind: K,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            blocker: BlockerKey::tag("t"),
        }
    }

    fn dummy_handoff() -> crate::action::HandoffAction<K> {
        crate::action::HandoffAction {
            kind: K,
            prompt: crate::handoff_prompt::HandoffPrompt::new("h"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingHuman,
            blocker: BlockerKey::tag("t"),
        }
    }

    #[test]
    fn outcome_maps_to_matching_exit_code_variant() {
        assert_eq!(
            Outcome::<K>::DoneSucceeded.exit_code(),
            ExitCode::DoneSucceeded
        );
        assert_eq!(Outcome::<K>::Paused.exit_code(), ExitCode::Paused);
        assert_eq!(
            Outcome::WouldAdvance(Box::new(dummy())).exit_code(),
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
        assert_eq!(Outcome::<K>::DoneAborted.exit_code(), ExitCode::DoneAborted);
        assert_eq!(
            Outcome::StuckRepeated(Box::new(dummy())).exit_code(),
            ExitCode::StuckRepeated
        );
        assert_eq!(
            Outcome::StuckCapReached(Box::new(dummy())).exit_code(),
            ExitCode::StuckCapReached
        );
        assert_eq!(
            Outcome::<K>::UsageError("bad".into()).exit_code(),
            ExitCode::UsageError
        );
        assert_eq!(
            Outcome::<K>::BinaryError("oops".into()).exit_code(),
            ExitCode::BinaryError
        );
    }

    #[test]
    fn halt_reason_maps_terminals_to_done_variants() {
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Succeeded
            ))),
            Outcome::DoneSucceeded
        ));
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::Terminal(
                Terminal::Aborted
            ))),
            Outcome::DoneAborted
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
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::AgentNeeded(
                dummy_handoff()
            ))),
            Outcome::HandoffAgent(_)
        ));
        assert!(matches!(
            Outcome::<K>::from(HaltReason::Decision(DecisionHalt::HumanNeeded(
                dummy_handoff()
            ))),
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
            Outcome::<K>::from(Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded))),
            Outcome::DoneSucceeded
        ));
    }

    #[test]
    fn binary_error_constructor_wraps_in_single_line_string() {
        // The flatten-newlines invariant is owned by
        // SingleLineString; this just verifies that the
        // constructor routes through it.
        let o: Outcome<K> = Outcome::binary_error("line one\nline two");
        match o {
            Outcome::BinaryError(s) => assert_eq!(s.as_str(), "line one line two"),
            _ => panic!("expected BinaryError"),
        }
    }

    #[test]
    fn usage_error_constructor_wraps_in_single_line_string() {
        let o: Outcome<K> = Outcome::usage_error("oops\nbad flag");
        match o {
            Outcome::UsageError(s) => assert_eq!(s.as_str(), "oops bad flag"),
            _ => panic!("expected UsageError"),
        }
    }

    // ─── Outcome serialization schema goldens ───────────────────────
    //
    // The recorder writes `Outcome` to `outcome.json` via serde. The
    // serialized shape is part of the on-disk caller contract — any
    // variant rename or structural change here breaks downstream
    // tooling. The exhaustive match in `outcome_serialization_golden`
    // is the contract: adding a new `Outcome` variant fails to
    // compile here until a golden arm is added.
    //
    // The Action payload's `kind` field serializes via K's
    // `Serialize` impl. The test `K` is a unit struct (serializes as
    // `null`); per-binary tests cover the real-K serialization via
    // their own recorder goldens.

    fn outcome_serialization_golden(o: &Outcome<K>) -> serde_json::Value {
        use serde_json::json;
        // Mechanical variants (Stuck*, WouldAdvance) carry a full
        // `Action` with an `effect` field; handoff variants carry a
        // `HandoffAction` with a top-level `prompt` field instead
        // (the prompt is a direct member, not nested inside an
        // `ActionEffect` enum). These two inner shapes capture the
        // canonical serde output so the variant goldens stay short.
        let dummy_action_json = json!({
            "kind": null,
            "effect": {"Full": {"log": "x"}},
            "target_effect": "Blocks",
            "urgency": "BlockingFix",
            "blocker": "t",
        });
        let dummy_handoff_json = json!({
            "kind": null,
            "prompt": {"headline": "h", "sections": []},
            "target_effect": "Blocks",
            "urgency": "BlockingHuman",
            "blocker": "t",
        });
        match o {
            // Unit variants serialize as a bare variant-name string.
            Outcome::DoneSucceeded => json!("DoneSucceeded"),
            Outcome::Paused => json!("Paused"),
            Outcome::DoneAborted => json!("DoneAborted"),
            // Tuple variants wrapping an Action serialize as
            // `{"VariantName": <Action json>}`. Box<Action> is
            // transparent via Box's Serialize impl.
            Outcome::StuckRepeated(_) => json!({"StuckRepeated": dummy_action_json}),
            Outcome::StuckCapReached(_) => json!({"StuckCapReached": dummy_action_json}),
            Outcome::WouldAdvance(_) => json!({"WouldAdvance": dummy_action_json}),
            Outcome::HandoffHuman(_) => json!({"HandoffHuman": dummy_handoff_json}),
            Outcome::HandoffAgent(_) => json!({"HandoffAgent": dummy_handoff_json}),
            // Tuple variants wrapping a SingleLineString serialize
            // as `{"VariantName": "msg"}` — SingleLineString
            // serializes transparently as a String.
            Outcome::BinaryError(s) => json!({"BinaryError": s.as_str()}),
            Outcome::UsageError(s) => json!({"UsageError": s.as_str()}),
        }
    }

    /// One sample `Outcome` per variant. Hand-maintained; the length
    /// sentinel in `outcome_serialization_goldens_exhaustive` catches
    /// drift.
    fn outcome_variant_samples() -> Vec<Outcome<K>> {
        vec![
            Outcome::DoneSucceeded,
            Outcome::Paused,
            Outcome::DoneAborted,
            Outcome::StuckRepeated(Box::new(dummy())),
            Outcome::StuckCapReached(Box::new(dummy())),
            Outcome::WouldAdvance(Box::new(dummy())),
            Outcome::HandoffHuman(Box::new(dummy_handoff())),
            Outcome::HandoffAgent(Box::new(dummy_handoff())),
            Outcome::BinaryError("err".into()),
            Outcome::UsageError("bad flag".into()),
        ]
    }

    #[test]
    fn outcome_serialization_goldens_exhaustive() {
        let samples = outcome_variant_samples();
        assert_eq!(
            samples.len(),
            10,
            "`outcome_variant_samples` must include one sample per \
             `Outcome` variant; adding a new variant requires adding \
             both an arm in `outcome_serialization_golden` AND a \
             sample here.",
        );
        for outcome in samples {
            let actual = serde_json::to_value(&outcome).unwrap();
            let expected = outcome_serialization_golden(&outcome);
            assert_eq!(actual, expected, "schema mismatch for {outcome:?}");
        }
    }
}
