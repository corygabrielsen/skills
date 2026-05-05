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
//! ∃ ProcessOutcome with BinaryError(_)          → 6
//! ∃ ProcessOutcome with HandoffAgent(_)         → 5
//! ∃ ProcessOutcome with HandoffHuman(_)         → 3
//! ∃ ProcessOutcome with StuckCapReached(_)      → 2
//! ∃ ProcessOutcome with StuckRepeated(_)        → 1
//! ∃ ProcessOutcome with WouldAdvance(_)         → 4    (inspect-only)
//! all terminal/Paused (DoneMerged | DoneClosed | Paused) → 0
//! ```
//!
//! `Paused` (per-PR exit 7) and `DoneClosed` (per-PR exit 8) fold
//! into `0` at the suite level: they are non-actionable terminal
//! states from the harness's perspective. The per-PR records on
//! stdout disambiguate.
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

use serde::Serialize;

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::outcome::Outcome;

/// Single-PR result inside a suite. Field order mirrors the input
/// `(slug, pr)` pair plus the per-PR `Outcome` of driving it.
#[derive(Debug, Serialize)]
pub struct ProcessOutcome {
    pub slug: RepoSlug,
    pub pr: PullRequestNumber,
    pub outcome: Outcome,
}

/// Suite-level binary boundary. Constructed in `main` from the parser
/// (UsageError) or from the suite spawn loop (Bundle).
#[derive(Debug, Serialize)]
pub enum MultiOutcome {
    /// Parser-time failure; no PRs ran.
    UsageError(String),
    /// All PRs reached a halt state (terminal, handoff, stuck, or
    /// per-PR BinaryError). The `Vec` is in input order; suite
    /// invariants (`|bundle| ≥ 1`, distinct `(slug, pr)` pairs) hold
    /// when the parser produced a valid `Suite`.
    Bundle(Vec<ProcessOutcome>),
}

impl MultiOutcome {
    /// Aggregate exit-code projection. See module-level docs for the
    /// priority table.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::UsageError(_) => 64,
            Self::Bundle(prs) => bundle_exit_code(prs),
        }
    }
}

fn bundle_exit_code(prs: &[ProcessOutcome]) -> u8 {
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::BinaryError(_)))
    {
        return 6;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::HandoffAgent(_)))
    {
        return 5;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::HandoffHuman(_)))
    {
        return 3;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::StuckCapReached(_)))
    {
        return 2;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::StuckRepeated(_)))
    {
        return 1;
    }
    if prs
        .iter()
        .any(|p| matches!(p.outcome, Outcome::WouldAdvance(_)))
    {
        return 4;
    }
    // Remaining variants — DoneMerged, DoneClosed, Paused — are
    // non-actionable terminal states at the suite level. Collapse
    // to 0; per-PR records on stdout disambiguate.
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::action::{Action, ActionKind, Automation, TargetEffect, Urgency};
    use crate::ids::BlockerKey;

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::parse(s).unwrap()
    }

    fn pr(n: u64) -> PullRequestNumber {
        PullRequestNumber::new(n).unwrap()
    }

    fn dummy_action() -> Action {
        Action {
            kind: ActionKind::Rebase,
            automation: Automation::Full,
            target_effect: TargetEffect::Blocks,
            urgency: Urgency::BlockingFix,
            description: "x".into(),
            blocker: BlockerKey::tag("rebase-needed"),
        }
    }

    fn record(s: &str, n: u64, o: Outcome) -> ProcessOutcome {
        ProcessOutcome {
            slug: slug(s),
            pr: pr(n),
            outcome: o,
        }
    }

    #[test]
    fn usage_error_maps_to_64() {
        let m = MultiOutcome::UsageError("bad invocation".into());
        assert_eq!(m.exit_code(), 64);
    }

    #[test]
    fn empty_bundle_is_zero() {
        // Structurally unreachable (parser enforces |suite| ≥ 1)
        // but `exit_code` is total — define the value rather than
        // panicking.
        let m = MultiOutcome::Bundle(vec![]);
        assert_eq!(m.exit_code(), 0);
    }

    #[test]
    fn all_done_merged_is_zero() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneMerged),
            record("a/b", 2, Outcome::DoneMerged),
        ]);
        assert_eq!(m.exit_code(), 0);
    }

    #[test]
    fn mixed_terminal_collapses_to_zero() {
        // DoneMerged + DoneClosed + Paused → all "no further action"
        // → 0 at suite level. Per-PR records disambiguate.
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneMerged),
            record("a/b", 2, Outcome::DoneClosed),
            record("a/b", 3, Outcome::Paused),
        ]);
        assert_eq!(m.exit_code(), 0);
    }

    #[test]
    fn would_advance_alone_is_4() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::WouldAdvance(dummy_action()),
        )]);
        assert_eq!(m.exit_code(), 4);
    }

    #[test]
    fn stuck_repeated_alone_is_1() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::StuckRepeated(dummy_action()),
        )]);
        assert_eq!(m.exit_code(), 1);
    }

    #[test]
    fn stuck_cap_reached_alone_is_2() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::StuckCapReached(dummy_action()),
        )]);
        assert_eq!(m.exit_code(), 2);
    }

    #[test]
    fn handoff_human_alone_is_3() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::HandoffHuman(dummy_action()),
        )]);
        assert_eq!(m.exit_code(), 3);
    }

    #[test]
    fn handoff_agent_alone_is_5() {
        let m = MultiOutcome::Bundle(vec![record(
            "a/b",
            1,
            Outcome::HandoffAgent(dummy_action()),
        )]);
        assert_eq!(m.exit_code(), 5);
    }

    #[test]
    fn binary_error_alone_is_6() {
        let m = MultiOutcome::Bundle(vec![record("a/b", 1, Outcome::BinaryError("oops".into()))]);
        assert_eq!(m.exit_code(), 6);
    }

    #[test]
    fn binary_error_beats_handoff_agent() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::HandoffAgent(dummy_action())),
            record("a/b", 2, Outcome::BinaryError("oops".into())),
        ]);
        assert_eq!(m.exit_code(), 6);
    }

    #[test]
    fn handoff_agent_beats_handoff_human() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::HandoffHuman(dummy_action())),
            record("a/b", 2, Outcome::HandoffAgent(dummy_action())),
        ]);
        assert_eq!(m.exit_code(), 5);
    }

    #[test]
    fn handoff_human_beats_stuck_cap_reached() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::StuckCapReached(dummy_action())),
            record("a/b", 2, Outcome::HandoffHuman(dummy_action())),
        ]);
        assert_eq!(m.exit_code(), 3);
    }

    #[test]
    fn stuck_cap_reached_beats_stuck_repeated() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::StuckRepeated(dummy_action())),
            record("a/b", 2, Outcome::StuckCapReached(dummy_action())),
        ]);
        assert_eq!(m.exit_code(), 2);
    }

    #[test]
    fn stuck_repeated_beats_would_advance() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::WouldAdvance(dummy_action())),
            record("a/b", 2, Outcome::StuckRepeated(dummy_action())),
        ]);
        assert_eq!(m.exit_code(), 1);
    }

    #[test]
    fn would_advance_beats_terminal() {
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneMerged),
            record("a/b", 2, Outcome::WouldAdvance(dummy_action())),
        ]);
        assert_eq!(m.exit_code(), 4);
    }

    #[test]
    fn full_priority_ordering_holds() {
        // Ten variants stacked in one bundle: BinaryError must win.
        let m = MultiOutcome::Bundle(vec![
            record("a/b", 1, Outcome::DoneMerged),
            record("a/b", 2, Outcome::DoneClosed),
            record("a/b", 3, Outcome::Paused),
            record("a/b", 4, Outcome::WouldAdvance(dummy_action())),
            record("a/b", 5, Outcome::StuckRepeated(dummy_action())),
            record("a/b", 6, Outcome::StuckCapReached(dummy_action())),
            record("a/b", 7, Outcome::HandoffHuman(dummy_action())),
            record("a/b", 8, Outcome::HandoffAgent(dummy_action())),
            record("a/b", 9, Outcome::BinaryError("e".into())),
        ]);
        assert_eq!(m.exit_code(), 6);
    }
}
