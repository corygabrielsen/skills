//! Decision types — what decide returns to the loop.
//!
//! Halt-as-predicate-not-scalar: the loop halts when there are no
//! advancing actions or when an action requires external resolution
//! (agent, human). Score is *not* part of the halt criterion; it's
//! a derived display value.

use super::action::Action;

/// What the loop should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Dispatch this action and re-iterate. Decide picked this from
    /// the candidate set; runtime semantics depend on `automation`.
    Execute(Action),
    /// Stop iterating. Surface the reason to the caller.
    Halt(HaltReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltReason {
    /// No actions to dispatch, no blockers — PR has reached its
    /// target state.
    Success,
    /// PR is in a terminal lifecycle state (merged or closed).
    Terminal(Terminal),
    /// Action requires an agent to execute. Loop halts; outer driver
    /// runs the agent and re-invokes.
    AgentNeeded(Action),
    /// Action requires a human (approve, push, etc.). Loop halts;
    /// outer driver surfaces and waits.
    HumanNeeded(Action),
    /// No actions emitted but blockers exist that none of our
    /// candidate generators can address. Should be rare; surface
    /// for investigation.
    Stalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Merged,
    Closed,
}
