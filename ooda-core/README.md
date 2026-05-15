# ooda-core

Shared boundary types and exit-code contract for the OODA skill
family in this repo. A small library crate (no I/O, no async, no
concurrency primitives) consumed by four sibling binaries:
`ooda-pr`, `ooda-prs`, `ooda-codex-review`, `ooda-pr-codex-review`.

## What this crate is

The four OODA binaries each drive an `observe → orient → decide →
act` loop over a different domain (one PR / N PRs / a `codex
review` ladder / a merged PR-plus-codex-review). The **boundary
shape** — what an invocation produces and how the caller dispatches
on `$?` — is identical across all four. `ooda-core` is that
shape, written once.

The crate exposes:

| Type                              | Role                                                                                                                      |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `Outcome<K>`                      | Binary boundary. 1:1 variant → exit-code (0–8, 64). Generic over a per-binary `ActionKind`.                               |
| `Decision<K>` / `DecisionHalt<K>` | Returned by `decide()`. Three-layered halt taxonomy (`Success` / `Terminal` / `AgentNeeded` / `HumanNeeded`).             |
| `HaltReason<K>`                   | Returned by `run_loop`. Superset of `DecisionHalt` with loop-only `Stalled` / `CapReached` variants.                      |
| `Terminal`                        | `Succeeded` \| `Aborted` — neutral verbs that fit every domain.                                                           |
| `Action<K>`                       | The operation `decide` prescribes. Carries `kind: K`, `automation`, `target_effect`, `urgency`, `description`, `blocker`. |
| `Automation`                      | `Full` \| `Wait{interval}` \| `Agent` \| `Human`.                                                                         |
| `Urgency`                         | `Critical < BlockingFix < BlockingWait < BlockingHuman < Advancing < Hygiene` (sort order for candidate actions).         |
| `TargetEffect`                    | `Blocks` \| `Advances` \| `Neutral`.                                                                                      |
| `BlockerKey`                      | Stable, non-empty stall-comparator key (newtype, no serde Deserialize).                                                   |
| `ActionKindName`                  | Trait each binary's `ActionKind` enum implements so the loop can render variant tokens.                                   |

## How a binary consumes it

Each binary defines its own `ActionKind` enum and creates
**concrete type aliases** over the generics:

```rust
// In ooda-pr/src/decide/action.rs:
pub use ooda_core::{ActionKindName, Automation, TargetEffect, Urgency};

pub type Action = ooda_core::Action<ActionKind>;

pub enum ActionKind {
    FixCi { check_name: CheckName },
    WaitForCi { pending: Vec<CheckName> },
    AddressThreads { threads: Vec<ReviewThread> },
    // … 19 more PR-domain variants
}

impl ActionKindName for ActionKind {
    fn name(&self) -> &'static str { /* … */ }
}

// In ooda-pr/src/outcome.rs:
pub type Outcome = ooda_core::Outcome<ActionKind>;
```

Existing call sites continue to write `Outcome::DoneSucceeded`,
`Action { kind, … }`, `Decision::Halt(DecisionHalt::Success)`
without seeing the generic parameter — type aliases are
transparent.

## Variant name vs stderr header

The Rust variant names are **internal**. The stderr header
strings are the **caller contract** and are emitted per-binary
by each binary's `render_outcome` function:

| `Outcome` variant | `ooda-pr` / `ooda-prs` / `ooda-pr-codex-review` stderr | `ooda-codex-review` stderr |
| ----------------- | ------------------------------------------------------ | -------------------------- |
| `DoneSucceeded`   | `DoneMerged`                                           | `DoneFixedPoint`           |
| `DoneAborted`     | `DoneClosed`                                           | `DoneAborted` _(reserved)_ |
| `Paused`          | `Paused`                                               | `Idle`                     |
| `StuckRepeated`   | `StuckRepeated: <ActionKind>:<BlockerKey>`             | same                       |
| `StuckCapReached` | `StuckCapReached: <ActionKind>:<BlockerKey>`           | same                       |
| `HandoffHuman`    | `HandoffHuman: <ActionKind>` + prompt block            | same                       |
| `WouldAdvance`    | `WouldAdvance: <ActionKind>:<Automation>`              | same _(reserved)_          |
| `HandoffAgent`    | `HandoffAgent: <ActionKind>` + prompt block            | same                       |
| `BinaryError`     | `BinaryError: <msg>`                                   | same                       |
| `UsageError`      | `UsageError: <msg>` + usage block                      | same                       |

This split is deliberate. It lets one type spine serve four
domains while letting each binary keep a stderr vocabulary that
fits its callers (`DoneMerged` reads naturally for PR work;
`DoneFixedPoint` reads naturally for codex-review ladder work).
The exit code is the formal contract; the stderr text is
domain-flavoured documentation.

## What stays per-binary

This crate intentionally **does not** lift:

- **`run_loop`** — each binary's iteration loop diverges on flock
  acquisition, head-SHA refresh, side-effect-mode dispatch, etc.
- **Recorder** — `ooda-pr` / `ooda-prs` / `ooda-pr-codex-review`
  share the PR-side state-root tree; `ooda-codex-review` uses a
  different one.
- **`From<LoopError> for Outcome`** — each binary's `LoopError`
  enum carries a different variant set; the PR-side enums hold
  `Observe` and `Act` variants, while the codex-side adds a
  `CodexObserve` variant.
- **`ActionKind` and `ActionKindName` impl** — these are the
  per-binary extension point. The trait is the witness that
  every domain enum provides a stable variant-name renderer.

The anti-DRY policy (see `feedback-anti-dry-mirror` in the user's
memory) still applies for everything outside the boundary types:
duplicated runner logic and recorder code are intentional until
the rule of three forces consolidation.

## Versioning and stability

The crate is unpublished and shared via path dependency
(`ooda-core = { path = "../ooda-core" }`). The 1:1 variant →
exit-code mapping is the contract; adding a new variant requires
allocating a new exit code (the unassigned range is 9–63).
Renaming an existing variant is allowed only if every binary's
stderr-emit table is checked for caller-contract impact — the
`Done*` rename in commit `dddaced` is the worked example.
