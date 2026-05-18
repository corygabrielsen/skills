//! Observe stage: fetch raw signals from external sources.
//!
//! Boundary: the input to this stage is wall-clock state on an
//! external system; the output is a bundle of unprojected facts.
//! Interpretation, classification, and policy live downstream.

pub(crate) mod branch;
pub(crate) mod codex;
pub(crate) mod github;
