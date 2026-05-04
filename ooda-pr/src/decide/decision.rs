//! Decision types — what decide returns to the loop.
//!
//! Halt-as-predicate-not-scalar: the loop halts when there are no
//! advancing actions or when an action requires external resolution
//! (agent, human). Score is *not* part of the halt criterion; it's
//! a derived display value.
//!
//! Two halt taxonomies, each total over its layer:
//!
//!   * [`DecisionHalt`] — what `decide()` can emit. Pure function of
//!     `OrientedState`; cannot observe loop-level events.
//!   * [`HaltReason`] — what `run_loop` returns. Strict superset:
//!     embeds `DecisionHalt` plus the loop-only `Stalled` and
//!     `CapReached` variants.
//!
//! Splitting them gives the compiler proof that render code (which
//! only ever sees decide-level halts) need not handle `Stalled` or
//! `CapReached`. The unified `HaltReason` shape would force dead
//! arms; the split eliminates them at the type level.
//!
//! Exit-code mapping lives on the types themselves
//! ([`Decision::exit_code`], [`HaltReason::exit_code`]), so the
//! taxonomy and its IPC encoding share one source of truth.

use super::action::Action;
use serde::Serialize;

/// What the loop should do next. Returned by [`super::decide`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Decision {
    /// Dispatch this action and re-iterate. Decide picked this from
    /// the candidate set; runtime semantics depend on `automation`.
    Execute(Action),
    /// Stop iterating. Surface the reason to the caller.
    Halt(DecisionHalt),
}

impl Decision {
    /// Documented exit-code mapping. `Execute` is `4` (in_progress):
    /// the full loop would auto-run the action, but a single-pass
    /// probe (`inspect`) does not — wrappers gating on success must
    /// see a non-zero exit so a still-advancing PR doesn't look
    /// green. See SKILL.md halt taxonomy.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Execute(_) => 4,
            Self::Halt(halt) => halt.exit_code(),
        }
    }
}

/// Why `decide()` returned a halt. Pure function of orient output;
/// no loop-level state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DecisionHalt {
    /// No actions to dispatch, no blockers — PR has reached its
    /// target state.
    Success,
    /// PR is in a terminal lifecycle state (merged or closed).
    Terminal(Terminal),
    /// Top candidate requires an agent to execute. Outer driver
    /// runs the agent and re-invokes.
    AgentNeeded(Action),
    /// Top candidate requires a human (approve, push, etc.). Outer
    /// driver surfaces and waits.
    HumanNeeded(Action),
}

impl DecisionHalt {
    /// Documented exit-code mapping. See SKILL.md halt taxonomy.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Success | Self::Terminal(_) => 0,
            Self::HumanNeeded(_) => 3,
            Self::AgentNeeded(_) => 5,
        }
    }

    /// Stable, finite, single-token rendering for the per-iteration
    /// halt log line (`[iter N] halt: <name>`). Distinct from
    /// `Debug` (which would dump full Action payloads and break
    /// the one-line-per-iteration diagnostic invariant).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::Terminal(Terminal::Merged) => "Terminal(Merged)",
            Self::Terminal(Terminal::Closed) => "Terminal(Closed)",
            Self::AgentNeeded(_) => "AgentNeeded",
            Self::HumanNeeded(_) => "HumanNeeded",
        }
    }
}

/// Why `run_loop` stopped. Superset of [`DecisionHalt`] with the
/// two loop-level halt classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum HaltReason {
    /// `decide()` produced a halt this iteration. Carries the
    /// underlying decide-level reason.
    Decision(DecisionHalt),
    /// Same `(kind, blocker)` action fired twice in a row without
    /// observable state change. Coarse stall detector — the
    /// iteration cap is the second line of defense. Carries the
    /// repeated action so callers can triage which loop step is
    /// stuck without re-deriving from logs.
    Stalled(Action),
    /// Iteration cap hit without halting. Re-run to continue, or
    /// raise `--max-iter`. Carries the last attempted action
    /// (Wait or non-Wait) — `runner` records every Execute's
    /// action as the diagnostic anchor; the cap can fire after a
    /// trailing Wait iteration. The Option arm of the prior shape
    /// was structurally unreachable when `--max-iter ≥ 1`
    /// (parser-validated) and at least one Execute fired;
    /// `runner` constructs this with a required Action.
    CapReached(Action),
}

impl HaltReason {
    /// Documented exit-code mapping. See SKILL.md halt taxonomy.
    /// Code `6` (runtime) and `64` (usage) live outside this enum:
    /// they describe loop *failure* (not halt) and CLI *parse*
    /// failure (not invocation), respectively.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Decision(halt) => halt.exit_code(),
            Self::Stalled(_) => 1,
            Self::CapReached(_) => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Terminal {
    Merged,
    Closed,
}
