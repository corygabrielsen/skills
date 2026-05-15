//! Decision types — what decide returns to the loop.
//!
//! Re-exports from [`ooda_core`] specialised to this binary's
//! [`super::action::ActionKind`]. The generic shapes and exit-code
//! mappings live in the shared crate; this module only fixes the
//! type parameter so call sites see ergonomic non-generic aliases.
//!
//! See [`ooda_core::decision`] for the three-layered halt taxonomy
//! and per-variant rationale.

use super::action::ActionKind;
pub use ooda_core::Terminal;

pub type Decision = ooda_core::Decision<ActionKind>;
pub type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub type HaltReason = ooda_core::HaltReason<ActionKind>;
