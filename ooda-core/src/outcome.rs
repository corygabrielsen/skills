//! Binary-boundary type: one variant per invocation, one exit code.
//!
//! Internal types ([`crate::Decision`], [`crate::HaltReason`], plus
//! per-binary loop errors) split halt-vs-execute and pure-decide vs
//! full-loop concerns. At the binary boundary those splits collapse
//! into a single sum type: callers want a single variant and a
//! single exit code per invocation.
//!
//! `Outcome<K>` is that boundary type; the 1:1 variant ↔ exit-code
//! bijection IS the wire contract.
//!
//! Construction:
//! * [`From<HaltReason<K>>`] — for full-loop callers.
//! * [`From<Decision<K>>`] — for single-pass / inspect callers.
//! * [`Outcome::binary_error`] — per-binary loop-error types funnel
//!   through here; their shapes are not uniform enough for a
//!   blanket `From`.
//! * Variants with no halt-source (CLI parse failure, paused) are
//!   constructed directly.

use crate::action::{Action, HandoffAction};
use crate::decision::{Decision, DecisionHalt, HaltReason, Terminal};
use crate::exit_code::ExitCode;
use crate::single_line_string::SingleLineString;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub enum Outcome<K> {
    /// Target reached its terminal success state. Per-binary
    /// renderers may emit a domain-specific header token; the
    /// internal variant name is domain-neutral.
    DoneSucceeded,
    /// Stall halt: same `(kind_name, blocker)` repeated on
    /// consecutive non-`Wait` iterations. Carries the repeated
    /// action so callers triage without re-deriving from logs.
    ///
    /// Action payload is boxed: `Outcome` flows as the `Err` arm
    /// of error-chain `Result`s, and keeping the variant small
    /// avoids `clippy::result_large_err` plus a fat success path.
    StuckRepeated(Box<Action<K>>),
    /// Iteration cap reached. Carries the last attempted action as
    /// the triage anchor. Boxed; see [`Self::StuckRepeated`].
    StuckCapReached(Box<Action<K>>),
    /// Halt requiring a human. Carries the handoff projection; the
    /// act stage did not run. Boxed; see [`Self::StuckRepeated`].
    HandoffHuman(Box<HandoffAction<K>>),
    /// Inspect mode only. Decide selected an executable action;
    /// the loop would have run it, inspect halts first. Boxed;
    /// see [`Self::StuckRepeated`].
    WouldAdvance(Box<Action<K>>),
    /// Halt requiring an agent. Same shape as [`Self::HandoffHuman`].
    HandoffAgent(Box<HandoffAction<K>>),
    /// Caught external failure (subprocess, network, IO). The
    /// [`SingleLineString`] type structurally enforces the
    /// "header is one line" contract on the diagnostic.
    BinaryError(SingleLineString),
    /// No candidate this pass; target is in-flight with no
    /// advancing work. May re-invoke later.
    Paused,
    /// Target reached a terminal non-success state. Per-binary
    /// renderers may emit a domain-specific header token.
    DoneAborted,
    /// CLI parse / validation failure. Diagnostic typed as
    /// [`SingleLineString`] per the same invariant as
    /// [`Self::BinaryError`].
    UsageError(SingleLineString),
    /// Loop polled `SHUTDOWN_SIGNAL` at an iteration boundary and
    /// observed a trapped `SIGINT` / `SIGTERM`. The wrapped exit
    /// code (`130` for `SIGINT`, `143` for `SIGTERM`) is the
    /// process-exit value; the loop owns the halt path so the
    /// recorder writes a terminal event and the live marker is
    /// released before exit.
    SignalInterrupted { exit_code: u8 },
}

impl<K> Outcome<K> {
    /// 1:1 variant ↔ exit-code projection. The wire contract.
    ///
    /// Returns [`ExitCode`] rather than a raw `u8` so the numeric
    /// values live only on the [`ExitCode`] enum; convert via
    /// `u8::from` / `i32::from` for [`std::process::exit`].
    #[must_use]
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
            // Project the wrapped u8 onto the matching typed variant.
            // Out-of-band values (the loop only stores 130 / 143)
            // fall back to `SignalSigterm` so the projection is
            // total — the typed enum stays the single source of
            // truth for shell-dispatch numerics.
            Self::SignalInterrupted { exit_code } => match *exit_code {
                130 => ExitCode::SignalSigint,
                _ => ExitCode::SignalSigterm,
            },
        }
    }

    /// Construct from a loop-error message. The single-line
    /// invariant on the stderr header is established by
    /// [`SingleLineString`] at the type boundary.
    pub fn binary_error(msg: impl Into<SingleLineString>) -> Self {
        Self::BinaryError(msg.into())
    }

    /// Construct from a CLI-validation diagnostic. Same invariant
    /// as [`Self::binary_error`].
    pub fn usage_error(msg: impl Into<SingleLineString>) -> Self {
        Self::UsageError(msg.into())
    }
}

/// Collapse the wider loop-halt taxonomy into the boundary type.
impl<K> From<HaltReason<K>> for Outcome<K> {
    fn from(reason: HaltReason<K>) -> Self {
        match reason {
            HaltReason::Decision(halt) => decision_halt_to_outcome(halt),
            HaltReason::Stalled(action) => Self::StuckRepeated(Box::new(action)),
            HaltReason::CapReached(action) => Self::StuckCapReached(Box::new(action)),
        }
    }
}

/// Collapse a single decide pass into the boundary type. An
/// `Execute` becomes [`Outcome::WouldAdvance`] since inspect mode
/// halts before acting; the halt arms share the loop-mode mapping.
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
    use crate::action::{Action, ActionEffect, MidTier, TargetEffect, Urgency};
    use crate::blocker::BlockerKey;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    struct K;

    fn dummy() -> Action<K> {
        Action {
            kind: K,
            effect: ActionEffect::Full {
                log: "x".into(),
                upstream: crate::action::UpstreamConsistency::Sync,
            },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("t"),
        }
    }

    fn dummy_handoff() -> crate::action::HandoffAction<K> {
        crate::action::HandoffAction {
            kind: K,
            prompt: crate::handoff_prompt::HandoffPrompt::new("h"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("t"),
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
        assert_eq!(
            Outcome::<K>::SignalInterrupted { exit_code: 130 }.exit_code(),
            ExitCode::SignalSigint
        );
        assert_eq!(
            Outcome::<K>::SignalInterrupted { exit_code: 143 }.exit_code(),
            ExitCode::SignalSigterm
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
            "effect": {"Full": {"log": "x", "upstream": "Sync"}},
            "target_effect": "Blocks",
            "urgency": {"Mid": "BlockingFix"},
            "blocker": "t",
        });
        let dummy_handoff_json = json!({
            "kind": null,
            "prompt": {"headline": "h", "sections": []},
            "target_effect": "Blocks",
            "urgency": {"Mid": "BlockingHuman"},
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
            // Struct-shaped variant — serde emits
            // `{"SignalInterrupted": {"exit_code": N}}`.
            Outcome::SignalInterrupted { exit_code } => json!({
                "SignalInterrupted": {"exit_code": exit_code},
            }),
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
            Outcome::SignalInterrupted { exit_code: 143 },
        ]
    }

    #[test]
    fn outcome_serialization_goldens_exhaustive() {
        let samples = outcome_variant_samples();
        assert_eq!(
            samples.len(),
            11,
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
