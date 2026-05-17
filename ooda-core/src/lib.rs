//! Shared spine for OODA-loop binaries.
//!
//! A binary drives observe → orient → decide → act over a chosen
//! domain. This crate fixes the cross-cutting boundary shape so
//! every such binary produces caller-visible artifacts with the
//! same algebra:
//!
//! * `Outcome<K>` — invocation result. The 1:1 variant ↔ exit-code
//!   bijection IS the wire contract; wrappers dispatch on `$?` alone.
//! * `Decision<K>` / `DecisionHalt<K>` / `HaltReason<K>` — layered
//!   halt taxonomy. The pure decide-pass returns the narrower
//!   `Decision`; the loop returns the wider `HaltReason`. Layering
//!   makes "cap / stall are loop-only" a compile-time fact rather
//!   than a documented convention.
//! * `Action<K>` — what decide prescribes. Generic over a per-binary
//!   action-kind enum (`K`).
//! * `ActionEffect` / `Urgency` / `TargetEffect` / `BlockerKey` —
//!   domain-agnostic fields of `Action`. `ActionEffect` fuses the
//!   dispatch discriminator with its correlated payload so the
//!   "handoff variants carry a prompt; in-loop variants carry a log
//!   line" class invariant is structural.
//!
//! # Scope
//!
//! No I/O, no async, no concurrency primitives. The crate is the
//! type spine plus its exit-code contract. Per-domain observe /
//! orient / decide / act / recorder layers live in the binaries
//! that supply `K`.

pub mod action;
pub mod atomic_io;
pub mod attest;
pub mod blocker;
pub mod current_manifest;
pub mod decision;
pub mod exit_code;
pub mod handoff_prompt;
pub mod non_empty;
pub mod outcome;
pub mod polling_interval;
pub mod pull_request_state;
pub mod rate_limit;
pub mod single_line_string;
pub mod stall_key;
pub mod state_root;

pub use action::{
    Action, ActionEffect, ActionKindName, HandoffAction, MidTier, TargetEffect, Urgency,
};
pub use blocker::{BlockerKey, BlockerKeyError, GateIdentity};
pub use current_manifest::{CurrentManifest, SCHEMA_VERSION as CURRENT_MANIFEST_SCHEMA_VERSION};
pub use decision::{
    Decision, DecisionHalt, HaltReason, Terminal, classify, decide_from_candidates,
};
pub use exit_code::ExitCode;
pub use handoff_prompt::{ContextLine, HandoffPrompt, PromptSection, Witness};
pub use non_empty::NonEmpty;
pub use outcome::Outcome;
pub use polling_interval::{PollingInterval, PollingIntervalError};
pub use pull_request_state::{PullRequestState, TerminalState};
pub use rate_limit::{BucketState, RateLimitBudget, RateLimitHit, RateLimitScope};
pub use single_line_string::SingleLineString;
pub use stall_key::StallKey;
