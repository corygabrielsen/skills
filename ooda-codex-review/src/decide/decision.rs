//! Decision types — what decide returns to the loop.
//!
//! Re-exports from [`ooda_core`] specialised to this binary's
//! [`super::action::ActionKind`]. The generic shapes, halt
//! taxonomy, and exit-code mappings live in the shared crate;
//! this module fixes the type parameter so call sites see
//! ergonomic non-generic aliases.
//!
//! Domain projection: `Terminal::Succeeded` denotes the per-target
//! review fixed point; the stderr renderer projects it to a
//! domain-specific header token (see this binary's outcome module).

use super::action::ActionKind;
pub(crate) use ooda_core::Terminal;

pub(crate) type Decision = ooda_core::Decision<ActionKind>;
pub(crate) type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub(crate) type HaltReason = ooda_core::HaltReason<ActionKind>;
