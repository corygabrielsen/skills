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

| Type                              | Role                                                                                                                                                                                                                                        |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Outcome<K>`                      | Binary boundary. Generic over a per-binary `ActionKind`. 1:1 variant → [`ExitCode`]; see "Exit-code scheme" below.                                                                                                                          |
| `ExitCode`                        | The numeric process-exit contract. `#[repr(u8)]` discriminants in one place; every typed result returns or takes this.                                                                                                                      |
| `Decision<K>` / `DecisionHalt<K>` | Returned by `decide()`. Three-layered halt taxonomy (`Success` / `Terminal` / `AgentNeeded` / `HumanNeeded`).                                                                                                                               |
| `HaltReason<K>`                   | Returned by `run_loop`. Superset of `DecisionHalt` with loop-only `Stalled` / `CapReached` variants.                                                                                                                                        |
| `Terminal`                        | `Succeeded` \| `Aborted` — neutral verbs that fit every domain.                                                                                                                                                                             |
| `Action<K>`                       | The operation `decide` prescribes. Carries `kind: K`, `automation`, `target_effect`, `urgency`, `description`, `blocker`.                                                                                                                   |
| `Automation`                      | `Full` \| `Wait{interval}` \| `Agent` \| `Human`.                                                                                                                                                                                           |
| `Urgency`                         | `Critical < BlockingFix < BlockingWait < BlockingHuman < Advancing < Hygiene < Closeout` (sort order for candidate actions; `Closeout` is the convergence-gate tier — outranked by every other tier so it fires only on global quiescence). |
| `TargetEffect`                    | `Blocks` \| `Advances` \| `Neutral`.                                                                                                                                                                                                        |
| `BlockerKey`                      | Stable, non-empty stall-comparator key (newtype, no serde Deserialize).                                                                                                                                                                     |
| `ActionKindName`                  | Trait each binary's `ActionKind` enum implements so the loop can render variant tokens.                                                                                                                                                     |

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

## Exit-code scheme

`Outcome::exit_code()` returns an [`ExitCode`] — a `#[repr(u8)]`
enum that holds the entire numeric contract. Call sites never
hardcode numbers; they pattern-match on `ExitCode::Variant`. The
numbers live exactly once, in `src/exit_code.rs`.

| Code | Variant         | Meaning                                                                                           |
| ---: | --------------- | ------------------------------------------------------------------------------------------------- |
|    0 | DoneSucceeded   | Terminal success (PR merged, codex ladder satisfied)                                              |
|    1 | Paused          | Loop completed this pass with no candidate action. Re-invoke later                                |
|    2 | WouldAdvance    | Inspect-only: would have run an action                                                            |
|    3 | HandoffHuman    | Handoff halt — caller must surface to a human                                                     |
|    4 | HandoffAgent    | Handoff halt — caller must dispatch an agent                                                      |
|    5 | DoneAborted     | Terminal non-success (PR closed without merge, ladder abandoned)                                  |
|    6 | StuckRepeated   | Escalation halt — same `(kind, blocker)` action fired twice consecutively                         |
|    7 | StuckCapReached | Escalation halt — iteration cap hit without halting                                               |
|   64 | UsageError      | BSD `sysexits.h` `EX_USAGE` — CLI parse failure                                                   |
|   70 | BinaryError     | BSD `sysexits.h` `EX_SOFTWARE` — caught internal failure (subprocess, IO)                         |
|  130 | _(reserved)_    | `SIGINT` (`128 + 2`). Synthesized by the shell on signal-kill; the binary itself never returns it |
|  143 | _(reserved)_    | `SIGTERM` (`128 + 15`). Same as `SIGINT`                                                          |

Codes `8–63` and `65–69` are deliberately unassigned. Adding a
new variant should either consume one of these slots (for a
genuinely new typed result) or adopt the appropriate `sysexits.h`
code (`EX_IOERR = 74`, `EX_TEMPFAIL = 75`, etc.) — never invent
a number for a category sysexits already names.

### Why these numbers

Three traditions converge in the scheme:

1. **POSIX shell + signals.** `0` for success; `128 + N` for
   signals. Non-negotiable.
2. **grep / diff / pytest** — _information-bearing low codes_.
   `1` is not "the tool broke"; it's "the tool worked and here
   is the result you asked for". `grep "needle"` exits `1` for
   no-match; pytest exits `1` when tests fail. `Paused` is the
   OODA family's analog: the loop ran, nothing needed driving,
   caller may invoke again later.
3. **BSD `sysexits.h`** (sendmail, 1993; adopted by `mail`,
   `postfix`, `systemd`, etc.). `64–78` are the closest thing
   the Unix world has to standardized typed-error codes:
   `EX_USAGE = 64`, `EX_SOFTWARE = 70`, `EX_IOERR = 74`,
   `EX_TEMPFAIL = 75`. The OODA binaries adopt `64` and `70`
   verbatim; future error categories should take other
   sysexits slots rather than squat on the low range.

Within the typed-halt block (`1–7`) the ordering is
**escalation-intensity ascending**: benign at the low end
(Paused, WouldAdvance), handoffs in the middle, escalation
halts at the high end. An agent reading `$?` can dispatch on
rough magnitude even without recalling each variant.

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
| `HandoffHuman`    | `HandoffHuman: <ActionKind>` + `  see: <handoff.md>`   | same                       |
| `WouldAdvance`    | `WouldAdvance: <ActionKind>:<Automation>`              | same _(reserved)_          |
| `HandoffAgent`    | `HandoffAgent: <ActionKind>` + `  see: <handoff.md>`   | same                       |
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
