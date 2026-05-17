//! Per-axis [`ooda_core::Axis`] impls for the PR domain.
//!
//! Lifts the existing per-axis `decide/*.rs` + `orient/*.rs` shape
//! into a uniform trait surface. Each impl is a thin wrapper around
//! the existing free-function code; no logic is duplicated.
//!
//! The driver-side composition (`AxisSet` over a tuple of impls,
//! topological projection over cross-axis deps) is the next arc;
//! this module establishes the per-axis shape only.

pub(crate) mod ci;
pub(crate) mod copilot;
pub(crate) mod cursor;
