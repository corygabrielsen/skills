//! Per-axis [`ooda_core::Axis`] impls for the PR domain.
//!
//! Lifts the existing per-axis `decide/*.rs` + `orient/*.rs` shape
//! into a uniform trait surface. Each impl is a thin wrapper around
//! the existing free-function code; no logic is duplicated.
//!
//! The driver-side composition (`AxisSet` over a tuple of impls,
//! topological projection over cross-axis deps) is the next arc;
//! this module establishes the per-axis shape only.
//!
//! # Two impl shapes
//!
//! - **Projection axes** ([`ci`], [`cursor`], [`copilot`]) own a
//!   per-axis observation slice + an internal projection step.
//!   The wrapper folds projection into
//!   [`ooda_core::Axis::candidates`] as a private helper and
//!   handles absent-observation short-circuiting.
//! - **Convergence axes** ([`state`], …) take a reference to the
//!   already-projected [`crate::orient::OrientedState`] (and a
//!   PR number where the underlying decide fn needs it). The
//!   wrapper is a one-line delegating call; cross-axis reads are
//!   the convergence axis's contract.
//!
//! # Test coverage
//!
//! Projection-axis wrappers each carry a smoke test because the
//! wrapper does non-trivial work (calls the projection helper,
//! short-circuits on `None`). Convergence-axis wrappers are pure
//! delegation — type-checked by the trait impl, behaviour-tested
//! by the underlying `decide::<axis>::candidates` tests. Adding
//! a per-wrapper smoke test for them would duplicate coverage
//! without exercising wrapper-specific code.

pub(crate) mod ci;
pub(crate) mod claude_review;
pub(crate) mod closeout;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod doc_review;
pub(crate) mod pull_request_metadata;
pub(crate) mod reviews;
pub(crate) mod state;
