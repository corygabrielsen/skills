//! Decision types — what decide returns to the loop.
//!
//! Fixes the shared-crate generic carrier to this binary's action
//! discriminant so call sites work in non-generic aliases. The
//! halt taxonomy and exit-code projection live upstream; see the
//! shared crate for variant-by-variant semantics.

use super::action::ActionKind;
pub(crate) use ooda_core::Terminal;

/// Driver dispatch signal — what the loop does next.
pub(crate) type Decision = ooda_core::Decision<ActionKind>;

/// Decide-level halt — the reasons emerging from candidate
/// generation alone.
pub(crate) type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;

/// Loop-level halt — superset that adds the loop-driver halt
/// classes (stall, cap).
pub(crate) type HaltReason = ooda_core::HaltReason<ActionKind>;
