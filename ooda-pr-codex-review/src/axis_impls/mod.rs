//! Per-axis [`ooda_core::Axis`] impls for the PR domain.
//!
//! Lifts the per-axis `decide/*.rs` + `orient/*.rs` shape into a
//! uniform trait surface. Each impl is a thin wrapper around the
//! existing free-function code; no logic is duplicated. Driver-side
//! composition lives in [`crate::runner::drive`], which dispatches
//! to each axis explicitly via the trait.
//!
//! # Uniform shape
//!
//! Every axis exposes:
//!
//! - `<X>Observation<'a>`: typed dep refs (declared-deps shape) —
//!   each field is exactly what `<X>Axis::candidates` reads. Optional
//!   axes (cursor, copilot) carry `Option<&Report>` to encode
//!   structural absence.
//! - `<X>Axis`: zero-sized impl of `Axis<<X>Observation<'a>>`. The
//!   body is a one-line delegation to `decide::<x>::candidates(...)`
//!   (or an absent-observation short-circuit for `Option`-shaped
//!   axes).
//!
//! Cross-axis deps appear as named fields on the observation struct
//! (e.g. `ReviewsObservation` carries `&CiReport` for the `ci_clean`
//! approval gate). The Driver site reads those deps from the local
//! per-axis projections and assembles each `<X>Observation`
//! inline — no shared mutable context.
//!
//! # Test coverage
//!
//! Projection-axis wrappers each carry a smoke test because the
//! wrapper handles absent-observation short-circuiting. Convergence
//! wrappers are pure delegation — type-checked by the trait impl,
//! behaviour-tested by the underlying `decide::<axis>::candidates`
//! tests. Per-wrapper smoke tests for them would duplicate coverage.

pub(crate) mod ci;
pub(crate) mod claude_review;
pub(crate) mod closeout;
pub(crate) mod copilot;
pub(crate) mod cursor;
pub(crate) mod doc_review;
pub(crate) mod pull_request_metadata;
pub(crate) mod reviews;
pub(crate) mod state;
