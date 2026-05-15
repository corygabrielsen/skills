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
//! * `Automation` / `Urgency` / `TargetEffect` / `BlockerKey` —
//!   domain-agnostic fields of `Action`.
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
pub mod blocker;
pub mod decision;
pub mod exit_code;
pub mod outcome;

pub use action::{Action, ActionKindName, Automation, TargetEffect, Urgency};
pub use blocker::{BlockerKey, BlockerKeyError};
pub use decision::{Decision, DecisionHalt, HaltReason, Terminal};
pub use exit_code::ExitCode;
pub use outcome::Outcome;
