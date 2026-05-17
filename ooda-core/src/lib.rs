//! Shared spine for the OODA skill family.
//!
//! Four binary skills in this repo (`ooda-pr`, `ooda-prs`,
//! `ooda-codex-review`, `ooda-pr-codex-review`) drive an
//! observe → orient → decide → act loop over different domains.
//! The *boundary* shapes are identical across all four:
//!
//! * `Outcome<K>` — what an invocation produces. 1:1 variant →
//!   exit-code mapping is the contract; wrappers dispatch on `$?`
//!   alone.
//! * `Decision<K>` / `DecisionHalt<K>` / `HaltReason<K>` — the
//!   three-layered halt taxonomy. `decide()` returns `Decision`;
//!   `run_loop` returns `HaltReason`. Splitting them gives the
//!   compiler proof that decide-level renderers need not handle
//!   `Stalled` / `CapReached`.
//! * `Action<K>` — the operation `decide` prescribes. Generic over
//!   the per-binary action-kind enum (`K`).
//! * `ActionEffect` / `Urgency` / `TargetEffect` / `BlockerKey` —
//!   domain-agnostic fields of `Action`. `ActionEffect` fuses the
//!   automation-kind discriminator with its correlated payload (a
//!   log line for `Full` / `Wait`; a structured `HandoffPrompt`
//!   for `Agent` / `Human`).
//!
//! Per-binary domain modules supply `K` (the action-kind enum,
//! implementing `ActionKindName`) and the observe / orient / decide
//! / act / recorder layers. Anti-DRY copy-paste between sibling
//! binaries continues for those layers; only the cross-cutting
//! boundary types live here.
//!
//! No I/O, no async, no concurrency primitives — this crate is the
//! type spine and exit-code contract, nothing else.

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

pub use action::{Action, ActionEffect, ActionKindName, HandoffAction, TargetEffect, Urgency};
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
