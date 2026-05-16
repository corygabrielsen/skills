//! Decision types — what decide returns to the loop.
//!
//! Re-exports from [`ooda_core`] specialised to this binary's
//! [`super::action::ActionKind`]. The generic shapes and exit-code
//! mappings live in the shared crate; this module only fixes the
//! type parameter so call sites see ergonomic non-generic aliases.
//!
//! See [`ooda_core::decision`] for the three-layered halt taxonomy
//! and per-variant rationale. `Terminal::Succeeded` covers the
//! codex-review ladder's fixed-point terminal state; the binary's
//! stderr renderer maps it to "`DoneFixedPoint`" for caller-visible
//! output.

use super::action::ActionKind;
pub(crate) use ooda_core::Terminal;

pub(crate) type Decision = ooda_core::Decision<ActionKind>;
pub(crate) type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub(crate) type HaltReason = ooda_core::HaltReason<ActionKind>;
