//! Layered halt taxonomy.
//!
//! A pure decide pass returns [`Decision<K>`] (Execute or a narrow
//! halt). A full loop returns [`HaltReason<K>`], which strictly
//! widens `Decision`'s halt set with the two loop-only halts (stall
//! detection, iteration cap). The layering makes "cap and stall are
//! loop-only" a compile-time fact: render code over the narrow type
//! cannot have dead match arms for them.
//!
//! Exit-code mapping is documented per-type
//! ([`Decision::exit_code`], [`HaltReason::exit_code`]) so the
//! taxonomy and its IPC encoding share one source of truth.

use crate::action::{Action, ActionEffect, HandoffAction};
use crate::exit_code::ExitCode;
use crate::pull_request_state::{PullRequestState, TerminalState};
use serde::Serialize;

/// Reduce a ranked candidate set under a PR-lifecycle state to a
/// [`Decision<K>`]. Lifecycle dominates: a terminal lifecycle maps
/// to the terminal halt regardless of candidates (there is nothing
/// to advance). On open lifecycle, an empty candidate set is
/// success; otherwise the top candidate is projected via
/// [`classify`].
pub fn decide_from_candidates<K>(
    candidates: Vec<Action<K>>,
    lifecycle: PullRequestState,
) -> Decision<K> {
    match lifecycle {
        PullRequestState::Terminal(TerminalState::Merged) => {
            return Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded));
        }
        PullRequestState::Terminal(TerminalState::Closed) => {
            return Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted));
        }
        PullRequestState::Open => {}
    }

    let Some(top) = candidates.into_iter().next() else {
        return Decision::Halt(DecisionHalt::Success);
    };

    classify(top)
}

/// Project an [`Action<K>`] onto a [`Decision<K>`] by partitioning
/// `effect` into "loop drives it" (Execute) and "external resolver
/// needed" (Halt with a [`HandoffAction`] projection). The
/// projection lifts `prompt` to a top-level field so decorators
/// reach it without pattern-matching past an uninhabitable arm.
pub fn classify<K>(action: Action<K>) -> Decision<K> {
    let Action {
        kind,
        effect,
        target_effect,
        urgency,
        blocker,
    } = action;
    match effect {
        ActionEffect::Full { log } => Decision::Execute(Action {
            kind,
            effect: ActionEffect::Full { log },
            target_effect,
            urgency,
            blocker,
        }),
        ActionEffect::Wait { interval, log } => Decision::Execute(Action {
            kind,
            effect: ActionEffect::Wait { interval, log },
            target_effect,
            urgency,
            blocker,
        }),
        ActionEffect::Agent { prompt } => {
            Decision::Halt(DecisionHalt::AgentNeeded(HandoffAction {
                kind,
                prompt,
                target_effect,
                urgency,
                blocker,
            }))
        }
        ActionEffect::Human { prompt } => {
            Decision::Halt(DecisionHalt::HumanNeeded(HandoffAction {
                kind,
                prompt,
                target_effect,
                urgency,
                blocker,
            }))
        }
    }
}

/// What the loop should do next. The pure-decide-pass result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Decision<K> {
    /// Dispatch this action and re-iterate. The action's
    /// `effect` discriminates how it is dispatched.
    Execute(Action<K>),
    /// Stop iterating. Surface the reason to the caller.
    Halt(DecisionHalt<K>),
}

impl<K> Decision<K> {
    /// Exit-code mapping. `Execute` maps to [`ExitCode::WouldAdvance`]
    /// so a single-pass inspect mode (which halts before acting)
    /// produces a nonzero `$?` — wrappers that gate on success
    /// must not see a still-advancing target as green.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Execute(_) => ExitCode::WouldAdvance,
            Self::Halt(halt) => halt.exit_code(),
        }
    }
}

/// Why a decide pass returned a halt. Pure function of its inputs;
/// no loop-level state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DecisionHalt<K> {
    /// No candidates and no blockers — target reached.
    Success,
    /// Target is in a terminal lifecycle state.
    Terminal(Terminal),
    /// Top candidate requires an agent. Carries a [`HandoffAction`]
    /// whose `prompt` is a top-level field (see [`classify`]).
    AgentNeeded(HandoffAction<K>),
    /// Top candidate requires a human. Shape parallels
    /// [`Self::AgentNeeded`].
    HumanNeeded(HandoffAction<K>),
}

impl<K> DecisionHalt<K> {
    /// Decide-level exit codes track [`crate::Outcome`]:
    /// `Success` and `Terminal(Succeeded)` halt the loop with
    /// [`ExitCode::DoneSucceeded`] (Paused is not produced at the
    /// decide layer); `Terminal(Aborted)` maps to
    /// [`ExitCode::DoneAborted`]; `HumanNeeded` maps to
    /// [`ExitCode::HandoffHuman`]; `AgentNeeded` maps to
    /// [`ExitCode::HandoffAgent`].
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Success | Self::Terminal(Terminal::Succeeded) => ExitCode::DoneSucceeded,
            Self::Terminal(Terminal::Aborted) => ExitCode::DoneAborted,
            Self::HumanNeeded(_) => ExitCode::HandoffHuman,
            Self::AgentNeeded(_) => ExitCode::HandoffAgent,
        }
    }

    /// Payload-free single-token rendering for log lines. Distinct
    /// from `Debug`, which would dump full payloads and break the
    /// one-line-per-iteration invariant.
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

/// Why the loop stopped. Strictly widens [`DecisionHalt`] with the
/// two loop-only halt classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum HaltReason<K> {
    /// A decide pass produced a halt this iteration. Carries the
    /// underlying narrow halt.
    Decision(DecisionHalt<K>),
    /// Stall: same `(kind_name, blocker)` fired on two consecutive
    /// non-`Wait` iterations. Carries the repeated action so
    /// callers can triage without re-deriving from logs.
    Stalled(Action<K>),
    /// Iteration cap reached without halting. Carries the last
    /// attempted action as the triage anchor.
    CapReached(Action<K>),
}

impl<K> HaltReason<K> {
    /// Exit-code mapping. CLI-parse failure and caught external
    /// failure are not halts — they live on [`crate::Outcome`]
    /// directly ([`ExitCode::UsageError`] / [`ExitCode::BinaryError`]).
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Decision(halt) => halt.exit_code(),
            Self::Stalled(_) => ExitCode::StuckRepeated,
            Self::CapReached(_) => ExitCode::StuckCapReached,
        }
    }
}

/// Domain-neutral terminal verdict. Each domain maps its own
/// success-shape onto `Succeeded` and its own abort-shape onto
/// `Aborted`; the enum carries no domain-specific vocabulary so a
/// single shape serves every binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Terminal {
    Succeeded,
    Aborted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, ActionEffect, MidTier, TargetEffect, Urgency};
    use crate::blocker::BlockerKey;
    use crate::handoff_prompt::HandoffPrompt;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    struct K;

    fn dummy() -> Action<K> {
        Action {
            kind: K,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("t"),
        }
    }

    fn dummy_handoff() -> HandoffAction<K> {
        HandoffAction {
            kind: K,
            prompt: HandoffPrompt::new("handoff"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("t"),
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
    fn decision_halt_terminal_succeeded_maps_to_done_succeeded() {
        let d: Decision<K> = Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded));
        assert_eq!(d.exit_code(), ExitCode::DoneSucceeded);
    }

    #[test]
    fn decision_halt_terminal_aborted_maps_to_done_aborted() {
        let d: Decision<K> = Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted));
        assert_eq!(d.exit_code(), ExitCode::DoneAborted);
    }

    #[test]
    fn decision_halt_handoffs_have_distinct_codes() {
        assert_eq!(
            DecisionHalt::<K>::Success.exit_code(),
            ExitCode::DoneSucceeded
        );
        assert_eq!(
            DecisionHalt::HumanNeeded(dummy_handoff()).exit_code(),
            ExitCode::HandoffHuman
        );
        assert_eq!(
            DecisionHalt::AgentNeeded(dummy_handoff()).exit_code(),
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
            HaltReason::Decision(DecisionHalt::HumanNeeded(dummy_handoff())).exit_code(),
            ExitCode::HandoffHuman
        );
        assert_eq!(
            HaltReason::Decision(DecisionHalt::AgentNeeded(dummy_handoff())).exit_code(),
            ExitCode::HandoffAgent
        );
    }

    fn dummy_with_effect(effect: ActionEffect) -> Action<K> {
        Action {
            kind: K,
            effect,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("t"),
        }
    }

    #[test]
    fn classify_full_yields_execute() {
        let d = classify(dummy_with_effect(ActionEffect::Full { log: "x".into() }));
        assert!(matches!(
            d,
            Decision::Execute(Action {
                effect: ActionEffect::Full { .. },
                ..
            })
        ));
    }

    #[test]
    fn classify_wait_yields_execute() {
        let d = classify(dummy_with_effect(ActionEffect::Wait {
            interval: crate::PollingInterval::from_secs(30),
            log: "x".into(),
        }));
        assert!(matches!(
            d,
            Decision::Execute(Action {
                effect: ActionEffect::Wait { .. },
                ..
            })
        ));
    }

    #[test]
    fn classify_agent_yields_agent_handoff() {
        let d = classify(dummy_with_effect(ActionEffect::Agent {
            prompt: HandoffPrompt::new("p"),
        }));
        assert!(matches!(d, Decision::Halt(DecisionHalt::AgentNeeded(_))));
    }

    #[test]
    fn decide_from_candidates_merged_yields_succeeded() {
        let d: Decision<K> = decide_from_candidates(
            vec![dummy()],
            PullRequestState::Terminal(TerminalState::Merged),
        );
        assert!(matches!(
            d,
            Decision::Halt(DecisionHalt::Terminal(Terminal::Succeeded))
        ));
    }

    #[test]
    fn decide_from_candidates_closed_yields_aborted() {
        let d: Decision<K> = decide_from_candidates(
            vec![dummy()],
            PullRequestState::Terminal(TerminalState::Closed),
        );
        assert!(matches!(
            d,
            Decision::Halt(DecisionHalt::Terminal(Terminal::Aborted))
        ));
    }

    #[test]
    fn decide_from_candidates_open_empty_yields_success() {
        let d: Decision<K> = decide_from_candidates(vec![], PullRequestState::Open);
        assert!(matches!(d, Decision::Halt(DecisionHalt::Success)));
    }

    #[test]
    fn decide_from_candidates_open_nonempty_delegates_to_classify() {
        let d: Decision<K> = decide_from_candidates(vec![dummy()], PullRequestState::Open);
        assert!(matches!(d, Decision::Execute(_)));
    }

    #[test]
    fn classify_human_yields_human_handoff() {
        let d = classify(dummy_with_effect(ActionEffect::Human {
            prompt: HandoffPrompt::new("p"),
        }));
        assert!(matches!(d, Decision::Halt(DecisionHalt::HumanNeeded(_))));
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
        assert_eq!(
            DecisionHalt::AgentNeeded(dummy_handoff()).name(),
            "AgentNeeded"
        );
        assert_eq!(
            DecisionHalt::HumanNeeded(dummy_handoff()).name(),
            "HumanNeeded"
        );
    }
}
