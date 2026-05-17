//! Multi-PR binary boundary type.
//!
//! `Outcome` is the per-PR boundary (one PR → one variant → one
//! exit code). `MultiOutcome` lifts that boundary to N PRs:
//!
//! ```text
//! MultiOutcome =
//!     UsageError(String)               -- parser failure; no PRs ran
//!   ⊕ Bundle(Vec⟨ProcessOutcome⟩)      -- every PR reached a halt state
//!
//! ProcessOutcome = (RepoSlug, PullRequestNumber, Outcome)
//! ```
//!
//! ## Priority projection
//!
//! `MultiOutcome::exit_code()` collapses the bundle to a single
//! `u8` for shell dispatch. The priority order is **highest first**:
//!
//! ```text
//! UsageError(_)                                 → 64
//! ∃ ProcessOutcome with BinaryError(_)          → 70
//! ∃ ProcessOutcome with HandoffAgent(_)         → 4
//! ∃ ProcessOutcome with HandoffHuman(_)         → 3
//! ∃ ProcessOutcome with StuckCapReached(_)      → 7
//! ∃ ProcessOutcome with StuckRepeated(_)        → 6
//! ∃ ProcessOutcome with WouldAdvance(_)         → 2    (inspect-only)
//! ∃ ProcessOutcome with DoneAborted(_)          → 5
//! all DoneSucceeded/Paused                       → 0
//! ```
//!
//! `Paused` (per-PR exit 1) folds into `0` at the suite level: it is
//! a non-actionable "no action this pass" outcome. `DoneAborted`
//! preserves its per-PR `5` at the suite level — a closed PR is a
//! distinct terminal state from a merged one, and the harness
//! caller routes on the difference. The per-PR records on stdout
//! disambiguate within each bucket.
//!
//! ## Totality
//!
//! `exit_code()` is a total function over `MultiOutcome`. The empty
//! `Bundle(vec![])` case is structurally rejected by the parser
//! (`|suite| ≥ 1`), but `exit_code` still defines a value (`0`) for
//! it so the function remains total at the type level.
//!
//! ## Design rationale
//!
//! Why not multi-record `$?`? Shell dispatch on `$?` is single-byte;
//! the harness needs *coarse* dispatch ("any agent work? any
//! errors?"). The fine-grained per-PR records belong on stdout (the
//! JSONL emitter), where they are parser-friendly. This split mirrors
//! the per-PR `Outcome` discipline: stderr for triage, `$?` for
//! dispatch, plus the new stdout channel for structured records.

use ooda_core::{ExitCode, SingleLineString};
use serde::Serialize;

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::outcome::Outcome;

/// Single-PR result inside a suite. Field order mirrors the input
/// `(slug, pr)` pair plus the per-PR `run_id` (opaque
/// [`ooda_state`] identifier joining this record back to the
/// on-disk `runs/<run-id>/` audit trail) and the per-PR `Outcome`
/// of driving it.
#[derive(Debug, Serialize)]
pub(crate) struct ProcessOutcome {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub run_id: String,
    pub outcome: Outcome,
}

/// Suite-level binary boundary. Constructed in `main` from the parser
/// (`UsageError`) or from the suite spawn loop (Bundle).
#[derive(Debug, Serialize)]
pub(crate) enum MultiOutcome {
    /// Parser-time failure; no PRs ran. [`SingleLineString`]
    /// enforces the no-newlines stderr-header invariant.
    UsageError(SingleLineString),
    /// All PRs reached a halt state (terminal, handoff, stuck, or
    /// per-PR `BinaryError`). The `Vec` is in input order; suite
    /// invariants (`|bundle| ≥ 1`, distinct `(slug, pr)` pairs) hold
    /// when the parser produced a valid `Suite`.
    Bundle(Vec<ProcessOutcome>),
}

impl MultiOutcome {
    /// Aggregate exit-code projection. See module-level docs for the
    /// priority table. Returns an [`ExitCode`] (re-exported from
    /// `ooda-core`) so the numeric values live in one place.
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::UsageError(_) => ExitCode::UsageError,
            Self::Bundle(prs) => bundle_exit_code(prs),
        }
    }
}

fn bundle_exit_code(prs: &[ProcessOutcome]) -> ExitCode {
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::BinaryError(_)))
    {
        return ExitCode::BinaryError;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::HandoffAgent(_)))
    {
        return ExitCode::HandoffAgent;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::HandoffHuman(_)))
    {
        return ExitCode::HandoffHuman;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::StuckCapReached(_)))
    {
        return ExitCode::StuckCapReached;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::StuckRepeated(_)))
    {
        return ExitCode::StuckRepeated;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::WouldAdvance(_)))
    {
        return ExitCode::WouldAdvance;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::DoneAborted))
    {
        return ExitCode::DoneAborted;
    }
    // Remaining variants — DoneSucceeded, Paused — are non-actionable
    // success/idle states at the suite level. Collapse to
    // DoneSucceeded (exit 0); per-PR records on stdout disambiguate.
    ExitCode::DoneSucceeded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{Action, ActionEffect, ActionKind, TargetEffect, Urgency};
    use crate::ids::BlockerKey;
    use ooda_core::MidTier;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::parse(s).unwrap()
    }

    fn pr(n: u64) -> PullRequestNumber {
        PullRequestNumber::new(n).unwrap()
    }

    fn dummy_action() -> Action {
        Action {
            kind: ActionKind::Rebase,
            effect: ActionEffect::Full { log: "x".into() },
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingFix),
            blocker: BlockerKey::from_static("rebase-needed"),
        }
    }

    fn dummy_handoff() -> ooda_core::HandoffAction<ActionKind> {
        ooda_core::HandoffAction {
            kind: ActionKind::RequestApproval,
            prompt: ooda_core::HandoffPrompt::new("h"),
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::Mid(MidTier::BlockingHuman),
            blocker: BlockerKey::from_static("not-approved"),
        }
    }

    fn record(s: &str, n: u64, o: Outcome) -> ProcessOutcome {
        ProcessOutcome {
            slug: slug(s),
            pr: pr(n),
            run_id: String::new(),
            outcome: o,
        }
    }

    #[test]
    fn usage_error_maps_to_64() {
        let m = MultiOutcome::UsageError("bad invocation".into());
        assert_eq!(m.exit_code(), ExitCode::UsageError);
    }

    #[test]
    fn empty_bundle_is_zero() {
        // Structurally unreachable (parser enforces |suite| ≥ 1)
        // but `exit_code` is total — define the value rather than
        // panicking.
        let m = MultiOutcome::Bundle(vec![]);
        assert_eq!(m.exit_code(), ExitCode::DoneSucceeded);
    }

    #[test]
    fn all_done_merged_is_zero() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneSucceeded),
            record("a/b", 2, Outcome::DoneSucceeded),
        ]);
        assert_eq!(m.exit_code(), ExitCode::DoneSucceeded);
    }

    #[test]
    fn mixed_terminal_with_done_aborted_is_five() {
        // DoneAborted is closed-without-merge; it is operationally
        // distinct from DoneSucceeded and from Paused (idle). The
        // bundle exit code surfaces the closure at the suite level.
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneSucceeded),
            record("a/b", 2, Outcome::DoneAborted),
            record("a/b", 3, Outcome::Paused),
        ]);
        assert_eq!(m.exit_code(), ExitCode::DoneAborted);
    }

    #[test]
    fn done_succeeded_and_paused_only_collapse_to_zero() {
        // Without any DoneAborted in the bundle, the remaining
        // success/idle states fold to DoneSucceeded.
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneSucceeded),
            record("a/b", 2, Outcome::Paused),
        ]);
        assert_eq!(m.exit_code(), ExitCode::DoneSucceeded);
    }

    #[test]
    fn done_aborted_alone_is_five() {
        let m = MultiOutcome::Bundle(vec![record("a/b", 1, Outcome::DoneAborted)]);
        assert_eq!(m.exit_code(), ExitCode::DoneAborted);
    }

    #[test]
    fn would_advance_beats_done_aborted() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneAborted),
            record("a/b", 2, Outcome::WouldAdvance(Box::new(dummy_action()))),
        ]);
        assert_eq!(m.exit_code(), ExitCode::WouldAdvance);
    }

    #[test]
    fn would_advance_alone_is_4() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::WouldAdvance(Box::new(dummy_action())),
        )]);
        assert_eq!(m.exit_code(), ExitCode::WouldAdvance);
    }

    #[test]
    fn stuck_repeated_alone_is_1() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::StuckRepeated(Box::new(dummy_action())),
        )]);
        assert_eq!(m.exit_code(), ExitCode::StuckRepeated);
    }

    #[test]
    fn stuck_cap_reached_alone_is_2() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::StuckCapReached(Box::new(dummy_action())),
        )]);
        assert_eq!(m.exit_code(), ExitCode::StuckCapReached);
    }

    #[test]
    fn handoff_human_alone_is_3() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::HandoffHuman(Box::new(dummy_handoff())),
        )]);
        assert_eq!(m.exit_code(), ExitCode::HandoffHuman);
    }

    #[test]
    fn handoff_agent_alone_is_5() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::HandoffAgent(Box::new(dummy_handoff())),
        )]);
        assert_eq!(m.exit_code(), ExitCode::HandoffAgent);
    }

    #[test]
    fn binary_error_alone_is_6() {
        let m = MultiOutcome::Bundle(vec![record("a/b", 1, Outcome::BinaryError("oops".into()))]);
        assert_eq!(m.exit_code(), ExitCode::BinaryError);
    }

    #[test]
    fn binary_error_beats_handoff_agent() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::HandoffAgent(Box::new(dummy_handoff()))),
            record("a/b", 2, Outcome::BinaryError("oops".into())),
        ]);
        assert_eq!(m.exit_code(), ExitCode::BinaryError);
    }

    #[test]
    fn handoff_agent_beats_handoff_human() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::HandoffHuman(Box::new(dummy_handoff()))),
            record("a/b", 2, Outcome::HandoffAgent(Box::new(dummy_handoff()))),
        ]);
        assert_eq!(m.exit_code(), ExitCode::HandoffAgent);
    }

    #[test]
    fn handoff_human_beats_stuck_cap_reached() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::StuckCapReached(Box::new(dummy_action()))),
            record("a/b", 2, Outcome::HandoffHuman(Box::new(dummy_handoff()))),
        ]);
        assert_eq!(m.exit_code(), ExitCode::HandoffHuman);
    }

    #[test]
    fn stuck_cap_reached_beats_stuck_repeated() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::StuckRepeated(Box::new(dummy_action()))),
            record("a/b", 2, Outcome::StuckCapReached(Box::new(dummy_action()))),
        ]);
        assert_eq!(m.exit_code(), ExitCode::StuckCapReached);
    }

    #[test]
    fn stuck_repeated_beats_would_advance() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::WouldAdvance(Box::new(dummy_action()))),
            record("a/b", 2, Outcome::StuckRepeated(Box::new(dummy_action()))),
        ]);
        assert_eq!(m.exit_code(), ExitCode::StuckRepeated);
    }

    #[test]
    fn would_advance_beats_terminal() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneSucceeded),
            record("a/b", 2, Outcome::WouldAdvance(Box::new(dummy_action()))),
        ]);
        assert_eq!(m.exit_code(), ExitCode::WouldAdvance);
    }

    #[test]
    fn full_priority_ordering_holds() {
        // Ten variants stacked in one bundle: BinaryError must win.
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneSucceeded),
            record("a/b", 2, Outcome::DoneAborted),
            record("a/b", 3, Outcome::Paused),
            record("a/b", 4, Outcome::WouldAdvance(Box::new(dummy_action()))),
            record("a/b", 5, Outcome::StuckRepeated(Box::new(dummy_action()))),
            record("a/b", 6, Outcome::StuckCapReached(Box::new(dummy_action()))),
            record("a/b", 7, Outcome::HandoffHuman(Box::new(dummy_handoff()))),
            record("a/b", 8, Outcome::HandoffAgent(Box::new(dummy_handoff()))),
            record("a/b", 9, Outcome::BinaryError("e".into())),
        ]);
        assert_eq!(m.exit_code(), ExitCode::BinaryError);
    }
}
