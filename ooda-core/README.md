# ooda-core

Shared boundary types, exit-code contract, and file primitives for
the OODA skill family in this repo. A library crate (no async)
consumed by the four OODA binaries — `ooda-pr`, `ooda-prs`,
`ooda-codex-review`, `ooda-pr-codex-review` — and by three
non-loop consumers: `ooda-state` (atomic file IO), `ooda-attest`
(attestation schema + bounded subprocess), and `converge`
(`ExitCode`).

## What this crate is

The four OODA binaries each drive an `observe → orient → decide →
act` loop over a different domain (one PR / N PRs / a `codex
review` ladder / a merged PR-plus-codex-review). The **boundary
shape** — what an invocation produces and how the caller dispatches
on `$?` — is identical across all four. `ooda-core` is that
shape, written once.

The crate exposes:

| Type                                                                                     | Role                                                                                                                                                                                                                                                                            |
| ---------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Outcome<K>`                                                                             | Binary boundary. Generic over a per-binary `ActionKind`. 1:1 variant → [`ExitCode`]; see "Exit-code scheme" below.                                                                                                                                                              |
| `ExitCode`                                                                               | The numeric process-exit contract. `#[repr(u8)]` discriminants in one place; every typed result returns or takes this.                                                                                                                                                          |
| `Decision<K>` / `DecisionHalt<K>`                                                        | Returned by `decide()`. Halt taxonomy `Success` \| `Terminal(Terminal)` \| `AgentNeeded(HandoffAction)` \| `HumanNeeded(HandoffAction)`.                                                                                                                                        |
| `decide_from_candidates` / `classify`                                                    | The shared halt predicate: lifecycle short-circuit, empty-candidate-set ⟹ `Success`, else `classify(top)` by its `ActionEffect`.                                                                                                                                                |
| `HaltReason<K>`                                                                          | Returned by `run_loop`. Superset of `DecisionHalt` with loop-only `Stalled(Action)` / `CapReached(Action)` variants.                                                                                                                                                            |
| `Terminal`                                                                               | `Succeeded` \| `Aborted` — neutral verbs that fit every domain.                                                                                                                                                                                                                 |
| `Action<K>`                                                                              | The operation `decide` prescribes. Carries `kind: K`, `effect`, `target_effect`, `urgency`, `blocker`.                                                                                                                                                                          |
| `HandoffAction<K>`                                                                       | `Action<K>` with `effect` replaced by `prompt: HandoffPrompt`. What the `Handoff*` outcome variants and `AgentNeeded` / `HumanNeeded` halts carry.                                                                                                                              |
| `ActionEffect`                                                                           | `Full{log, upstream: UpstreamConsistency}` \| `Wait{interval: PollingInterval, log}` \| `Agent{prompt}` \| `Human{prompt}`. Fuses the dispatch discriminator with its correlated payload so "handoff variants carry a prompt; in-loop variants carry a log line" is structural. |
| `UpstreamConsistency`                                                                    | `Sync` \| `Eventual(PollingInterval)`. Declared on `Full` effects; a runner grants one synthetic `Wait` to an `Eventual` repeat before calling it a stall.                                                                                                                      |
| `Urgency` / `MidTier`                                                                    | `Pre < Mid(MidTier) < Post`, `MidTier = Critical < Pathology < BlockingFix < BlockingWait < BlockingHuman < Advancing < Hygiene`. Sort key for candidate actions. `Pre` is first-time attestation; `Post` is the closeout convergence gate.                                     |
| `TargetEffect`                                                                           | `Blocks` \| `Advances` \| `Neutral`.                                                                                                                                                                                                                                            |
| `BlockerKey` / `GateIdentity`                                                            | Stable, non-empty stall-comparator key (newtype, `Serialize` only, no `Deserialize`). `GateIdentity` is the trait through which domain types produce one — `format!` into a key is a compile error.                                                                             |
| `StallKey`                                                                               | `(kind_name: &'static str, blocker: BlockerKey)` — payload-free projection of an `Action` that the stall comparator compares.                                                                                                                                                   |
| `ActionKindName`                                                                         | Trait each binary's `ActionKind` enum implements so the loop can render variant tokens.                                                                                                                                                                                         |
| `Axis`                                                                                   | Trait for per-axis candidate producers (`candidates(&Observation) → Vec<Action>`); binaries assemble their candidate set by fanning out over axis impls.                                                                                                                        |
| `HandoffPrompt`                                                                          | Structured prompt body (`headline` + sections) rendered into the handoff blob.                                                                                                                                                                                                  |
| `PullRequestState` / `TerminalState`                                                     | `Open` \| `Terminal(Merged \| Closed)` — the lifecycle input to `decide_from_candidates`.                                                                                                                                                                                       |
| `NonEmpty<T>`, `PollingInterval`, `SingleLineString`, `SafeBody`, `SafeUrl`, `CohortSha` | Validated value types shared by the boundary payloads.                                                                                                                                                                                                                          |
| `RateLimitBudget` / `RateLimitHit` / `RateLimitScope`                                    | Typed upstream-throttle signal an observe pass lifts into data instead of an error.                                                                                                                                                                                             |
| `attest`                                                                                 | On-disk attestation schema (`attested_sha`, `attested_at`, `version`) plus locked atomic read/write per axis. Single definition shared by the writer (`ooda-attest`) and the readers (the binaries' observe passes).                                                            |
| `atomic_io`, `file_lock`, `spawn`                                                        | `tmp+rename` writes with `0o700` directories; advisory `FileLock`; `run_with_limits` (deadline + per-stream byte caps) for subprocesses.                                                                                                                                        |

## How a binary consumes it

Each binary defines its own `ActionKind` enum and creates
**concrete type aliases** over the generics:

```rust
// In the consuming binary:
pub use ooda_core::{ActionEffect, ActionKindName, MidTier, TargetEffect, Urgency};

pub type Action  = ooda_core::Action<ActionKind>;
pub type Outcome = ooda_core::Outcome<ActionKind>;

pub enum ActionKind {
    // domain-specific funnel basins, e.g. for a PR-merge domain:
    FixCi { check_name: CheckName },
    WaitForCi { pending: NonEmpty<CheckName> },
    AddressThreads { threads: NonEmpty<ReviewThread> },
    // …
}

impl ActionKindName for ActionKind {
    fn name(&self) -> &'static str { /* … */ }
}
```

Call sites continue to write `Outcome::DoneSucceeded`,
`Action { kind, … }`, `Decision::Halt(DecisionHalt::Success)`
without seeing the generic parameter — type aliases are
transparent.

## Exit-code scheme

`Outcome::exit_code()` returns an [`ExitCode`] — a `#[repr(u8)]`
enum that holds the entire numeric contract. Call sites never
hardcode numbers; they pattern-match on `ExitCode::Variant`. The
numbers live exactly once, in `src/exit_code.rs`.

| Code | Variant           | Meaning                                                                                                                                       |
| ---: | ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
|    0 | DoneSucceeded     | Terminal success (PR merged, codex ladder satisfied)                                                                                          |
|    1 | Paused            | Loop completed this pass with no candidate action. Re-invoke later                                                                            |
|    2 | WouldAdvance      | Inspect-only: would have run an action                                                                                                        |
|    3 | HandoffHuman      | Handoff halt — caller must surface to a human                                                                                                 |
|    4 | HandoffAgent      | Handoff halt — caller must dispatch an agent                                                                                                  |
|    5 | DoneAborted       | Terminal non-success (PR closed without merge, ladder abandoned)                                                                              |
|    6 | StuckRepeated     | Escalation halt — same `StallKey` non-Wait action fired on consecutive non-Wait iterations (after any `Eventual` propagation window)          |
|    7 | StuckCapReached   | Escalation halt — iteration cap hit without halting                                                                                           |
|   64 | UsageError        | BSD `sysexits.h` `EX_USAGE` — CLI parse failure                                                                                               |
|   70 | BinaryError       | BSD `sysexits.h` `EX_SOFTWARE` — caught internal failure (subprocess, IO)                                                                     |
|  130 | SignalInterrupted | `SIGINT` (`128 + 2`). The loop traps the signal, finishes the iteration boundary, appends the terminal event, releases the live marker, exits |
|  143 | SignalInterrupted | `SIGTERM` (`128 + 15`). Same handling                                                                                                         |

`SignalInterrupted{exit_code}` carries the raw `u8`; `exit_code()`
projects `130` to `SignalSigint` and every other value to
`SignalSigterm`, so the projection is total. The shell synthesizes
the same `128 + N` for an untrapped kill, so a caller cannot
distinguish trapped from kernel paths on `$?` alone.

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

| `Outcome` variant   | `ooda-pr` / `ooda-prs` / `ooda-pr-codex-review` stderr        | `ooda-codex-review` stderr                        |
| ------------------- | ------------------------------------------------------------- | ------------------------------------------------- |
| `DoneSucceeded`     | `DoneMerged`                                                  | `DoneFixedPoint`                                  |
| `DoneAborted`       | `DoneClosed`                                                  | `DoneAborted`                                     |
| `Paused`            | `Paused`                                                      | `Idle`                                            |
| `StuckRepeated`     | `StuckRepeated: <ActionKind>:<BlockerKey>`                    | same                                              |
| `StuckCapReached`   | `StuckCapReached: <ActionKind>:<BlockerKey>`                  | same                                              |
| `HandoffHuman`      | `Hand off to human: <prompt headline>` + `  see: <blob path>` | `HandoffHuman: <ActionKind>` + `  prompt: <body>` |
| `HandoffAgent`      | `Hand off to agent: <prompt headline>` + `  see: <blob path>` | `HandoffAgent: <ActionKind>` + `  prompt: <body>` |
| `WouldAdvance`      | `WouldAdvance: <ActionKind>:<Effect>`                         | `WouldAdvance: <ActionKind>`                      |
| `BinaryError`       | `BinaryError: <msg>`                                          | same                                              |
| `UsageError`        | `UsageError: <msg>` + usage block                             | same                                              |
| `SignalInterrupted` | `Interrupted: exit code <130\|143>`                           | same                                              |

`ooda-prs` prefixes each per-PR block with a loop-identity tag;
the header grammar after the prefix is the `ooda-pr` column.

This split is deliberate. It lets one type spine serve four
domains while letting each binary keep a stderr vocabulary that
fits its callers (`DoneMerged` reads naturally for PR work;
`DoneFixedPoint` reads naturally for codex-review ladder work).
The exit code is the formal contract; the stderr text is
domain-flavoured documentation.

## What stays per-binary

This crate intentionally **does not** lift code whose shape
diverges across binaries:

- **Iteration loop** — each binary's loop diverges on
  side-channel acquisition (e.g. advisory locks), upstream
  state refresh between iterations, and side-effect-mode
  dispatch. Lifting prematurely would force a union shape over
  divergent concerns.
- **Recorder** — the event vocabulary is per-domain. The on-disk
  layout (`runs/<id>/events.jsonl` + `blobs/`, `live/<id>`
  markers) and state-root resolution live in `ooda-state`; there
  is one state root per machine and domain identity is carried
  inside events, not in the path.
- **`From<LoopError> for Outcome`** — each binary's `LoopError`
  enum carries the union of error sources its loop can witness;
  the conversion to `Outcome::BinaryError` is per-binary because
  the union differs.
- **`ActionKind` and its `ActionKindName` impl** — the per-binary
  extension point. The trait is the witness that every domain
  enum provides a stable variant-name renderer; the enum itself
  encodes the domain's funnel basins.
- **Observe / orient / decide / act layers** — including the
  `Axis` impls. PR-domain lifts that would be shared only by the
  PR trio do not belong here; `ooda-core` hosts boundary types
  and domain-agnostic primitives only.

The anti-DRY policy still applies for everything outside the
boundary types: duplicated runner and recorder code are
intentional until the rule of three forces consolidation.

## Versioning and stability

The crate is unpublished and shared via path dependency. The
1:1 variant → exit-code mapping is the contract; adding a new
variant requires allocating a new exit code from the unassigned
range (`8–63`, `65–69`). Renaming an existing variant is
permitted only after auditing every binary's stderr-emit table
for caller-contract impact, because callers may grep the stderr
header even though dispatch is by `$?` alone.
