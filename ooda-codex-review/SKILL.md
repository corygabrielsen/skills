---
name: ooda-codex-review
description: Drive `codex review` to fixed point across the reasoning ladder. Two invocation modes — loop (observe → orient → decide → optionally act → emit one Outcome) and side-effect (emit one ladder-transition event, then emit one Outcome). Each invocation is one self-contained run in the OODA state tree (`runs/<run-id>/events.jsonl` + `blobs/`); no state is carried across invocations. The orchestrator dispatches on the exit code; for `HandoffAgent` (exit 4) and `HandoffHuman` (exit 3) it also reads the action name from the stderr header to choose a branch.
args:
  - name: --uncommitted
    description: Mode flag. Review working-tree changes vs HEAD. The four mode flags (`--uncommitted`, `--base`, `--commit`, `--pr`) are mutually exclusive; exactly one is required for any non-help invocation (loop or side-effect). `--help` / `-h` is the sole exception — it requires no mode flag and short-circuits before any other validation. Passing two or more mode flags raises `UsageError`.
  - name: --base BRANCH
    description: Mode flag. Review the current branch vs BRANCH.
  - name: --commit SHA
    description: Mode flag. Review one commit by 40-hex SHA.
  - name: --pr NUM
    description: Mode flag. Review a PR's changes by number. Loop mode resolves the PR's base branch with `gh pr view NUM --json baseRefName --jq .baseRefName` and invokes the current `codex review` CLI as `codex review --base <baseRefName>`. The caller must already be in the intended PR worktree/branch; the binary does not checkout or mutate branches. The mode + number land on the `run_started` event's `target` payload (`{"mode":"pr","value":"<NUM>",...}`).
  - name: --level LVL
    description: "Configured floor for this invocation. One of low|medium|high|xhigh. Default low. Recorded on the `run_started` event's `target.floor` field; orchestrators that walk the ladder across invocations pass the current rung via `--level` each time."
  - name: --ceiling LVL
    description: User-configured upper bound of the climb (distinct from the ladder edge xhigh, which is the absolute top of the reasoning ladder). One of low|medium|high|xhigh (same token set as `--level`). All-clean at this level halts `DoneFixedPoint` directly without a Retrospective handoff. Default xhigh, set by the binary's CLI parser. Must be >= --level (UsageError otherwise). Levels are totally ordered low < medium < high < xhigh.
  - name: -n N
    description: Parallel review count. Default 3, must be ≥1. Loop-mode only; not part of any cross-invocation key.
  - name: --max-iter N
    description: Loop-iteration cap. Default 50, must be ≥1. Silently ignored by side-effect invocations (NOT a UsageError — `--max-iter` is one of the loop-mode-only knobs; see Side-effect mode below).
  - name: --state-root PATH
    description: OODA state-tree root. Default resolves via $OODA_STATE_HOME → $XDG_STATE_HOME/ooda → $HOME/.local/state/ooda → $TMPDIR/ooda. Every invocation creates a fresh `runs/<run-id>/` subtree here; no on-disk state is shared across invocations.
  - name: --codex-bin PATH
    description: Path to the `codex` binary. Default `codex` (PATH lookup).
  - name: --criteria STRING
    description: "Reserved but currently unsupported. The current `codex review` CLI rejects positional prompts when combined with target modes (`--uncommitted`, `--base`, `--commit`), so this binary fails fast with `UsageError` whenever `--criteria` is passed. Omit it and use codex's built-in review criteria."
  - name: --mark-retro-clean
    description: |
      Side-effect. Orchestrator reports the retrospective at the
      current level produced no architectural changes. Always
      records the per-level outcome (variant `Clean`) before
      dispatching (the recording happens regardless of which
      dispatch row fires, including the no-op rows). Dispatch is
      a partition of `current_level` against the configured
      ceiling and the ladder edge xhigh — only Row 2's
      `== configured ceiling` test halts; the other three rows
      (Row 1 is a pure ordering test against the configured ceiling;
      Rows 3 and 4 combine an ordering test against the configured
      ceiling with an edge test against the ladder edge xhigh) all
      yield `Idle`:

      | `current_level`                                 | Behavior                                          | Outcome                   |
      | :---------------------------------------------- | :------------------------------------------------ | :------------------------ |
      | strictly below configured ceiling               | advance one rung up the ladder (toward the ceiling) | `Idle` (exit 1)         |
      | == configured ceiling                           | no level change; halt terminal-success            | `DoneFixedPoint` (exit 0) |
      | strictly above configured ceiling AND < xhigh   | advance one rung up the ladder (further above the ceiling, NOT back toward it) | `Idle` (exit 1) |
      | == xhigh AND `current != configured ceiling`    | no level change (ladder edge no-op)               | `Idle` (exit 1)           |

      Rows are evaluated as a partition of `current_level` against
      the configured ceiling — Row 2's strict equality takes
      precedence; Rows 3 and 4 only fire when current strictly
      exceeds the configured ceiling. Rows 3 and 4 are only
      reachable when out-of-band `--advance-level` pushed
      `current_level` above the configured ceiling (or when
      `--mark-retro-clean` itself overshoots from an already-above
      state — see the recovery note below). To return to the
      canonical ladder, call `--restart-from-floor` (which resets
      current_level to the configured floor); a single
      `--mark-retro-clean` will not emit `DoneFixedPoint` from any
      above-ceiling state.
  - name: --mark-retro-changes REASON
    description: "Side-effect. Orchestrator reports the retrospective surfaced architectural changes (the orchestrator should implement those changes BEFORE invoking this flag — the flag closes the level out and resets `current_level` to the configured floor, it does not gate the implementation). Records the per-level outcome (variant `RetrospectiveChanges`) with REASON, then sets `current_level` to the configured floor (when `current_level` is already at the floor this is a no-op transition but the outcome still records — same shape as `--restart-from-floor`'s at-floor render). Above-ceiling state (out-of-band, e.g. after `--advance-level` overshoot) is handled identically: the reset always lands at the configured floor regardless of pre-mutation level, and the resolution-line `<LEVEL>` slots reflect the pre-mutation value. Outcome: `Idle` (exit 1)."
  - name: --mark-address-passed
    description: "Side-effect. Orchestrator reports the address agent fixed the batch and the project's tests passed. Records an `AddressPassed` event and computes a one-rung drop from `--level`; at floor the drop is a no-op (the resolution line says `no drop`). Outcome: `Idle` (exit 1). Rationale (per the loop-codex-review meta-procedure this binary implements): drop one level after a passing fix so the next iteration re-runs reviews at lower reasoning to confirm the patch passes simpler scrutiny too."
  - name: --mark-address-failed DETAILS
    description: "Side-effect. Orchestrator reports post-address tests failed. Emits a `HandoffHuman` (exit 3); the stderr action kind on the header line is `TestsFailedTriage`. The prompt is a fixed template that interpolates the pre-invocation `--level` and DETAILS verbatim: `Tests failed after addressing review batch at level <LEVEL>. Surface to a human for triage. Details: <DETAILS>`. The prompt body is also written to a content-addressed blob in the run's `blobs/` for audit. Unlike the recording marks, this side-effect writes nothing to stdout."
  - name: --advance-level
    description: "Side-effect (low-level primitive). Records an `AdvanceLevel` event computing `current_level.higher()` against the value passed via `--level`. Bounded by the **ladder edge xhigh**; no-op at xhigh emits an `at ladder edge (<LEVEL>); no advance` line. Outcome `Idle` (exit 1) on success and on ladder-top no-op. Per the one-invocation-one-run model, this flag computes and records the would-be transition but does NOT persist `current_level` for the next invocation — the orchestrator passes the resulting level in via the next `--level`."
  - name: --drop-level
    description: "Side-effect (low-level primitive). Records a `DropLevel` event computing `current_level.lower()` against the value passed via `--level`. Clamped at the configured floor; no-op at floor emits an `at floor (<LEVEL>); no drop` line. Outcome `Idle` (exit 1) on success and on floor no-op. Per the one-invocation-one-run model, this flag computes and records the would-be transition but does NOT persist `current_level` for the next invocation — the orchestrator passes the resulting level in via the next `--level`."
  - name: --restart-from-floor
    description: "Side-effect (low-level primitive). Records a `RestartFromFloor` event. The resolution line is always emitted; per the one-invocation-one-run model the floor IS the level passed via `--level`, so this renders e.g. as `restarted from floor: low -> low`. Outcome `Idle` (exit 1). Prefer `--mark-retro-changes` for the canonical retrospective-changes flow (which also records the restart and additionally captures a REASON string); use `--restart-from-floor` only when no retrospective context applies (tests, recovery)."
  - name: -h, --help
    description: >
      Print usage to stdout and exit 0. Standalone short-circuit;
      not an invocation mode and does not require a mode flag. To
      distinguish from `DoneFixedPoint` (also exit 0) the canonical
      method is to check for `--help` / `-h` in the argv the
      orchestrator itself issued — see "Two invocation modes" below.
      As a defensive fallback (e.g. for orchestrators that don't track
      their own argv), checking whether stderr is empty also works —
      every non-`--help` Outcome (including `DoneFixedPoint` from
      either loop or side-effect mode) writes a header to stderr, so
      empty stderr uniquely identifies `--help`. Prefer the argv check
      when feasible: it is local to the orchestrator and does not
      depend on capturing stderr.
---

# /ooda-codex-review

Drives `codex review` to fixed point across the reasoning ladder
(low → medium → high → xhigh). The reasoning content lives in the
orchestrator (the outer Claude session), not in this binary. The
binary is a stateless step function: spawn `codex review`
subprocesses, poll their log files, halt with structured handoffs
when an LLM is needed (verify-and-address, retrospective synthesis).

> **Notation.** Throughout this document, `<NAME>` (angle brackets)
> denotes a placeholder that the binary substitutes at runtime
> (e.g. `<LEVEL>` becomes `low`/`medium`/`high`/`xhigh`,
> `<DETAILS>` becomes the orchestrator-supplied string). Angle
> brackets are never literal in the binary's output.

## Type spine

Boundary types are defined in the `ooda-core` library crate
(`/home/cory/code/skills/ooda-core/`) and shared with the three
sibling OODA binaries. This binary depends on `ooda-core` via
path dep and instantiates each generic type over its
domain-specific `ActionKind` enum — the codex-review-domain
variant set (`RunReviews`, `AwaitReviews`, `ParseVerdicts`,
`AddressBatch`, `Retrospective`, `AdvanceLevel`, `DropLevel`,
`RestartFromFloor`, `RunTests`, `RequestCriteriaRefinement`,
`TestsFailedTriage`):

```rust
pub type Outcome      = ooda_core::Outcome<ActionKind>;
pub type Decision     = ooda_core::Decision<ActionKind>;
pub type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub type HaltReason   = ooda_core::HaltReason<ActionKind>;
pub type Action       = ooda_core::Action<ActionKind>;
```

`Automation`, `Urgency`, `TargetEffect`, `BlockerKey`, `Terminal`,
and the `ActionKindName` trait are re-exported from `ooda-core`.
`ReasoningLevel` (the codex-review ladder type) stays per-binary
in `decide/action.rs`.

**Variant name ≠ stderr header.** Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are internal — neutral
verbs shared across the binary family. Stderr header strings
(`DoneFixedPoint`, `DoneAborted`, `Idle`) are this binary's
caller contract, emitted by the per-binary `render_outcome`
function. The Outcomes table below shows both representations.
The mapping is:

| Variant name    | Stderr header    | Exit |
| --------------- | ---------------- | :--: |
| `DoneSucceeded` | `DoneFixedPoint` |  0   |
| `DoneAborted`   | `DoneAborted`    |  8   |
| `Paused`        | `Idle`           |  7   |

**Per-binary code (not lifted):** `runner.rs::run_loop` (the
codex-side runner carries the per-iteration `EventSink` driving
`ooda-state`), `decide/action.rs::ActionKind` and its
`ActionKindName` impl, and `From<LoopError> for Outcome`. The
on-disk state model is the domain-neutral `ooda-state` crate;
see its docs for the events.jsonl + blobs layout.

See `ooda-core/README.md` and `ooda-core/src/lib.rs` for the
shared-spine design rationale.

## Two invocation modes

A single binary serves two distinct flows; both produce one `Outcome`
with one exit code.

- **Loop mode** — the default when no side-effect flag is set. The
  binary internally repeats observe → orient → decide → optionally
  act, up to `--max-iter` iterations, until any single iteration
  produces an Outcome that ends the invocation. Decide returning no
  candidate action ends the invocation immediately with the `Idle`
  Outcome (exit 1) — the binary does NOT keep iterating internally
  once decide produces no candidate; it returns to the orchestrator
  after the first such iteration and lets the orchestrator decide
  whether to re-invoke. (Note: a `Wait` action like `AwaitReviews`
  IS a candidate action and does NOT trigger this exit; only the
  literal "decide produced no action at all" condition does.) The full set of post-parse loop-mode Outcomes is:
  `DoneFixedPoint`, `StuckRepeated`, `StuckCapReached`,
  `HandoffAgent`, `BinaryError`, or `Idle` (a malformed argv
  surfaces `UsageError` instead, before mode dispatch — see the
  `UsageError` orthogonality note below). Loop mode never returns
  `HandoffHuman` today (only `--mark-address-failed` produces it);
  `WouldAdvance` and `DoneAborted` are reserved exit codes the
  binary cannot currently produce. After the binary returns, the
  orchestrator's outer loop re-invokes the binary — for
  `HandoffAgent` after the dispatched agent + follow-up `--mark-*`
  complete; for `Idle` immediately to drive the next observe; for
  `DoneFixedPoint`, `StuckRepeated`, `StuckCapReached`, and
  `BinaryError`, the orchestrator stops the loop and surfaces the
  result.

  `UsageError` is orthogonal to loop vs side-effect: it can fire
  from any invocation if the CLI argv is malformed, before mode
  dispatch.

- **Side-effect mode** — triggered by `--mark-*` or the low-level
  `--advance-level` / `--drop-level` / `--restart-from-floor`.
  Skips the OODA loop entirely; creates a fresh run with a single
  `iteration_decided` (or `iteration_handoff` for
  `--mark-address-failed`) event recording the requested
  transition, and emits the documented Outcome. The seven
  side-effect flags split into three classes by intent:

  | Class                     | Flags                                                                 |                        Computes a ladder transition?                        |
  | ------------------------- | --------------------------------------------------------------------- | :-------------------------------------------------------------------------: |
  | **Recording marks**       | `--mark-retro-clean`, `--mark-retro-changes`, `--mark-address-passed` | usually (see per-flag descriptions for ceiling- and floor-edge no-op cases) |
  | **Transition primitives** | `--advance-level`, `--drop-level`, `--restart-from-floor`             |                   usually (no-op at ladder edge / floor)                    |
  | **Escalation**            | `--mark-address-failed`                                               |                       never (purely surfaces a halt)                        |

  Side-effect flags are mutually exclusive with each other
  (UsageError if combined). Loop-mode-only knobs are silently
  parsed and ignored alongside a side-effect flag when they are
  otherwise valid; today this means `--max-iter`. `--criteria` is
  not valid in any mode until the current `codex review` CLI
  supports prompts with target modes. All non-help invocations
  require a mode flag for the `run_started` event's `target`
  payload (the orchestrator picks the mode + `--level`; the
  binary records both verbatim).

  **No persisted manifest.** Per the one-invocation-one-run state
  model, side-effect flags do NOT mutate any value the next
  invocation will read. They COMPUTE the requested transition
  against the `--level` the orchestrator passes in (treating it
  as the pre-mutation value), record the transition as a single
  event in this run's `events.jsonl`, log the resolution line to
  stdout, and exit. The orchestrator owns ladder position across
  invocations and supplies it via `--level` each time.

  **BinaryError tolerance.** Every side-effect call documents an
  expected non-error Outcome (Idle, DoneFixedPoint, or HandoffHuman),
  but the underlying state-tree IO can surface `BinaryError`
  (exit 70). Orchestrators MUST treat exit 70 as a possible
  additional outcome for every `--mark-*` and primitive call. The
  orchestration recipe encapsulates this in the
  `expect_or_binary_error` helper.

**The configured floor and ceiling.** `--level` (default `low`)
is the floor passed to this invocation; `--ceiling` (default
`xhigh`) is its ceiling. Side-effect invocations that compute a
transition relative to either should re-pass the same values the
loop invocation used so the binary's edge checks (ladder-edge,
floor-clamp, ceiling-equals) line up — but neither value is
persisted between invocations, so a mismatch only affects the
single invocation that drifted.

A standalone help mode also exists: `--help` / `-h` prints the
usage text to stdout and exits 0 (it produces no Outcome header on
stderr). Help is not a "mode" in the same sense as loop or
side-effect — it is a standalone short-circuit. Orchestrators that
dispatch on `$?` should distinguish help from `DoneFixedPoint` by
detecting `--help` in the argv they themselves issued, not by exit
code (help should never appear in an orchestration loop).

## Architecture: where the smarts live

Each row is one step in the per-level cycle. Action terminology:
**Full** actions run synchronously inside the binary; **Wait**
actions yield (sleep, then re-observe — Wait actions never trigger
the StuckRepeated detector).

| Step                                                                                                                                                                                                         | Procedural (this binary)                                                                                                                                                                                                                                                                                                                                  | LLM (orchestrator)                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Discover repo root                                                                                                                                                                                           | `git rev-parse --show-toplevel`                                                                                                                                                                                                                                                                                                                           |                                                                                                                                                              |
| Spawn n codex reviews                                                                                                                                                                                        | `RunReviews` (Full) writes `<LEVEL>-<slot>.log` synchronously, spawns codex through a wrapper, and records `<LEVEL>-<slot>.exit` when the child finishes                                                                                                                                                                                                  |                                                                                                                                                              |
| Poll log/exit files for marker                                                                                                                                                                               | `AwaitReviews` (Wait; 30s default)                                                                                                                                                                                                                                                                                                                        |                                                                                                                                                              |
| Extract verdict + classify                                                                                                                                                                                   | `scan_batch` heuristic                                                                                                                                                                                                                                                                                                                                    |                                                                                                                                                              |
| Verify + address batch, then run tests (test execution is orchestrator-owned; the binary itself never spawns tests)                                                                                          | binary emits the `HandoffAgent: AddressBatch` Outcome (header on stderr, then a one-line prompt body with `<COUNT>` and `<LEVEL>` substituted; the prompt body says e.g. "Verify and address 2 review(s) with issues at level low...") and exits                                                                                                          | `HandoffAgent: AddressBatch` → orchestrator runs the agent + tests, reports via `--mark-address-passed` or `--mark-address-failed`                           |
| Synthesize retrospective (and, if the agent surfaces patterns, the orchestrator implements them BEFORE invoking `--mark-retro-changes` — the flag closes the level out, it does not gate the implementation) | binary emits the `HandoffAgent: Retrospective` Outcome — header on stderr followed by the prompt template — and exits                                                                                                                                                                                                                                     | `HandoffAgent: Retrospective` → orchestrator runs the agent, reports via `--mark-retro-clean` (no patterns) or `--mark-retro-changes` (patterns implemented) |
| Climb / drop / restart                                                                                                                                                                                       | each side-effect invocation computes the transition against `--level`, records it as a single-event run via `ooda-state`, and exits — the recording marks (`--mark-retro-clean` / `--mark-retro-changes` / `--mark-address-passed`) and the low-level primitives (`--advance-level` / `--drop-level` / `--restart-from-floor`) share the same event shape |                                                                                                                                                              |

The poll cadence is 30s by default; the `OODA_AWAIT_SECS` env var
overrides it (intended for tests; orchestrators should leave it
unset in production).

## Outcomes (exit codes)

The boundary contract is **one variant → one exit code**. The
orchestrator dispatches on `$?` for the coarse routing decision; for
`HandoffAgent` (exit 4) and `HandoffHuman` (exit 3) it also reads
the action name from the first line of stderr to pick a branch (see
"Streams contract" below). For all other Outcomes, dispatch is
complete from `$?` alone. Some of those Outcomes do carry
structured stderr (e.g. `StuckRepeated: <ActionKind>:<blocker key>`
and `StuckCapReached: <ActionKind>:<blocker key>` encode the
recurring or last-attempted action), but the orchestrator does not
need to parse it to pick a branch — it is diagnostic detail that
helps an operator interpret the halt, and orchestrators that wish
to log or alert on these tokens may parse them.

| Code | Variant           | Producer(s)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| ---- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 0    | `DoneSucceeded`   | Stderr header `DoneFixedPoint`. Loop mode: a per-level batch completes with all reviews clean while `current_level == configured ceiling` exactly (strict equality; an above-ceiling all-clean state — reachable only via out-of-band `--advance-level` — produces `HandoffAgent: Retrospective` instead, see exit 4). Side-effect mode: `--mark-retro-clean` while `current_level == configured ceiling` exactly (per Row 2 of its dispatch table; above-ceiling cases produce `Paused` / stderr `Idle`).                                                                                                   |
| 1    | `Paused`          | Stderr header `Idle`. Loop mode: decide had nothing to do this iteration. Side-effect mode: any of `--advance-level`, `--drop-level`, `--restart-from-floor`, `--mark-retro-clean` (when current is at any level **other than** the configured ceiling — both below AND above), `--mark-retro-changes`, `--mark-address-passed` completing successfully.                                                                                                                                                                                                                                                     |
| 2    | `WouldAdvance`    | **Reserved**, not producible. Future inspect-mode (single-pass observe + decide, no act) would map a non-halt decision into this code. There is currently no CLI surface to invoke it.                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 3    | `HandoffHuman`    | Side-effect: `--mark-address-failed`. (Loop mode never selects a `HandoffHuman` action today; this is the only producer.)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| 4    | `HandoffAgent`    | Loop mode: a per-level batch completes with at least one review flagging issues → `AddressBatch` (regardless of current level vs. ceiling); a per-level batch completes with all reviews clean while `current_level != configured ceiling` → `Retrospective` (whether current is below OR strictly above ceiling — above-ceiling is reachable via out-of-band `--advance-level`). The stderr header carries the action name (`AddressBatch` or `Retrospective`); the orchestrator reads it to branch.                                                                                                        |
| 5    | `DoneAborted`     | **Reserved**, not producible. The variant exists for future SIGINT / abort handling; no trigger is wired today.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| 6    | `StuckRepeated`   | Loop mode: the same `(action kind, blocker key)` pair fired on two consecutive non-Wait iterations (Wait iterations are skipped over when evaluating "consecutive"). Wait actions (e.g. `AwaitReviews` polling) are excluded from this check; cycling on Wait is normal. The stderr header carries the action kind and blocker key of the repeated action. The set of action kinds that can recur this way is narrowed by the architecture: today only `RunReviews` qualifies (handoff actions exit before they could recur, Wait actions are excluded). See the `<ActionKind>` placeholder reference below. |
| 7    | `StuckCapReached` | Loop mode: `--max-iter` iterations elapsed without a halt. The stderr header carries the action kind and blocker key of the **last attempted action** (which may be a Wait, since the cap can fire on a polling iteration).                                                                                                                                                                                                                                                                                                                                                                                  |
| 64   | `UsageError`      | CLI parse / validation failure (unknown flag, invalid value, conflicting flags).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| 70   | `BinaryError`     | BSD sysexits `EX_SOFTWARE`. Caught external failure. Triggers split by mode: codex spawn failure, nonzero codex exit status, and malformed completed codex logs are loop-mode only (side-effect mode never spawns codex); IO errors and state-tree write failures are producible from BOTH modes (every non-help invocation opens a state-tree run, so any `--mark-*` or low-level primitive can return `BinaryError` if the state tree is unwritable). The header line is `BinaryError: <message>`.                                                                                                         |
| 130  | _(reserved)_      | `SIGINT` (`128 + 2`). Synthesized by the shell when the process is signal-killed; the binary itself never returns this.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| 143  | _(reserved)_      | `SIGTERM` (`128 + 15`). Same handling as `SIGINT`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |

## Streams contract

| Stream | Always carries                                                                                         | Sometimes carries                                                                                                                                                                                                                                                                          |
| ------ | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| stdout | nothing for loop mode                                                                                  | help text (for `--help`); a one-line resolution message before the Outcome header for the six side-effect flags that emit one (`--mark-address-failed` is the exception — it writes nothing to stdout). See "Side-effect resolution lines" below.                                          |
| stderr | the Outcome header (one line) for every Outcome (`--help` has no Outcome and writes nothing to stderr) | an indented `  prompt: <description>` line — always present for `HandoffAgent` and `HandoffHuman` Outcomes, never present for the others. `UsageError` additionally carries the full multi-line usage block on stderr (printed after the header so orchestrators can show it to the user). |
| `$?`   | the exit code (the machine contract)                                                                   |                                                                                                                                                                                                                                                                                            |

### Stderr header format per variant

The Outcome header is always the first line on stderr, but its
shape varies by variant:

| Variant           | Header line                                                                                                                                       |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `DoneSucceeded`   | `DoneFixedPoint`                                                                                                                                  |
| `Paused`          | `Idle`                                                                                                                                            |
| `DoneAborted`     | `DoneAborted` _(reserved; not currently producible)_                                                                                              |
| `StuckRepeated`   | `StuckRepeated: <ActionKind>:<blocker key>`                                                                                                       |
| `StuckCapReached` | `StuckCapReached: <ActionKind>:<blocker key>`                                                                                                     |
| `HandoffHuman`    | `HandoffHuman: <ActionKind>` followed by `  prompt: <description>` line (today only `TestsFailedTriage` is emitted, from `--mark-address-failed`) |
| `HandoffAgent`    | `HandoffAgent: <ActionKind>` followed by `  prompt: <description>` line (`AddressBatch` or `Retrospective`)                                       |
| `WouldAdvance`    | `WouldAdvance: <ActionKind>` _(reserved; not currently producible)_                                                                               |
| `BinaryError`     | `BinaryError: <message>`                                                                                                                          |
| `UsageError`      | `UsageError: <message>` followed by the full usage block (multi-line, written to stderr)                                                          |

### Placeholder reference

Used in the templates below, in the prompt template at
`--mark-address-failed`, and in the stderr header table above.
Angle brackets are placeholders, never literal in the binary's
output.

| Placeholder     | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| :-------------- | :---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `<ActionKind>`  | The decide-layer action name. The set of `<ActionKind>` tokens that can appear in stderr headers today is fixed and small — orchestrators should branch on the values listed here and treat any unrecognized token as opaque. **Producible today:** `HandoffAgent` → `AddressBatch` or `Retrospective`; `HandoffHuman` → `TestsFailedTriage` (only emitted by the `--mark-address-failed` side-effect — never selected by the loop's decide layer); `StuckRepeated` → `RunReviews` (only — see narrowing note below); `StuckCapReached` → either `RunReviews` or the Wait kind `AwaitReviews` (cap can fire on a polling iteration). **Reserved (header format defined but not currently emitted):** `WouldAdvance` reserves an `<ActionKind>` slot but is not currently producible. **Carries no `<ActionKind>`:** `DoneFixedPoint`, `Idle`, `BinaryError`, `UsageError`, `DoneAborted`. **`StuckRepeated` narrowing:** the `StuckRepeated` row in the Outcomes table (exit 6) says it fires on "two consecutive non-Wait iterations of the same `(action kind, blocker key)` pair". The set of decide-layer actions that are non-halting AND non-Wait is exactly `{RunReviews}` today: handoff actions like `AddressBatch` and `Retrospective` are also non-Wait but produce a halt Outcome that exits the invocation, so they cannot recur within a single binary invocation; Wait actions like `AwaitReviews` execute on each iteration but are excluded from the StuckRepeated equality check, so the "two consecutive non-Wait iterations" rule is evaluated by skipping over any interleaved Wait iterations. Hence `StuckRepeated` always carries `RunReviews` today. **Reserved future decide-layer variants (in the `ActionKind` enum, no producer today):** `ParseVerdicts`, `AdvanceLevel`, `DropLevel`, `RestartFromFloor`, `RequestCriteriaRefinement`. The names `AdvanceLevel` / `DropLevel` / `RestartFromFloor` mirror the side-effect CLI flags but are independent enum variants — the side-effect flags themselves are NOT decide-layer actions, they are CLI surfaces handled by side-effect dispatch. **Excluded from future decide-layer use:** the enum also carries a `RunTests` variant for shape-symmetry with the procedural pipeline, but `RunTests` is permanently NOT a decide-layer action — running tests is permanently the orchestrator's job (see "What this binary does NOT do" below). Decide will never emit `RunTests`, and `RunTests` will never appear in any stderr header. |
| `<LEVEL>`       | A reasoning-level token (`low` / `medium` / `high` / `xhigh`). Substituted with the `--level` value the orchestrator passed in (or `low` if omitted). For side-effect resolution lines that compute a one-rung transition (`--mark-retro-clean`, `--mark-address-passed`, `--advance-level`, `--drop-level`), `<LEVEL>` is the pre-transition value and `<NEXT_LEVEL>` (when present) is the post-transition value computed by the binary.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `<NEXT_LEVEL>`  | The reasoning-level token after a one-rung transition. Direction depends on the template: advance → one rung up; drop → one rung down. The placeholder name is direction-agnostic. Used by the one-rung templates (`--advance-level`, `--drop-level`, `--mark-address-passed`, and `--mark-retro-clean` for its non-edge advance case); restart-style templates substitute `<FLOOR>` directly for their destination slot since the destination is always the configured floor — no `<NEXT_LEVEL>` slot appears in `--restart-from-floor` or `--mark-retro-changes` resolution lines.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `<FLOOR>`       | The configured floor (`--level`'s value, or `low` if `--level` was omitted).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `<CEILING>`     | The configured ceiling (`--ceiling`'s value, or `xhigh` if `--ceiling` was omitted).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `<COUNT>`       | The number of reviewers in the current batch whose verdict classified as has-issues, used only by the `AddressBatch` handoff's prompt body (e.g. `Verify and address 2 review(s) with issues at level low`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `<REASON>`      | Free-form orchestrator-supplied REASON string passed to `--mark-retro-changes`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `<DETAILS>`     | Free-form orchestrator-supplied DETAILS string passed to `--mark-address-failed`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `<message>`     | Caught-error message embedded in `BinaryError: <message>`. Covers every `BinaryError` trigger source from the Outcomes table: codex spawn failure, codex child exit/log failure, IO error, state-tree open/append failure.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `<blocker key>` | Stable iteration-key string the binary uses to detect `StuckRepeated`. The same key shape also appears in `StuckCapReached` headers (it carries the last-attempted action's key, which may be a Wait). Free-form per-action; orchestrators should treat the suffix after `<ActionKind>:` as opaque.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `<description>` | The prompt body emitted on the `  prompt:` line (two-space indent) for `HandoffAgent` and `HandoffHuman` Outcomes. For `HandoffAgent: AddressBatch` it is the AddressBatch prompt template (one line today; see Stderr example below). For `HandoffAgent: Retrospective` it is the Retrospective prompt template. For `HandoffHuman: TestsFailedTriage` it is the fixed template at the `--mark-address-failed` arg description (with `<LEVEL>` and `<DETAILS>` interpolated).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |

### Side-effect resolution lines (format templates)

Six of the seven side-effect flags emit a single advisory line to
stdout before the Outcome header lands on stderr.
`--mark-address-failed` is the exception: it emits nothing on
stdout (only the `HandoffHuman: TestsFailedTriage` header on
stderr).

The format templates below use the placeholders defined above.
Runtime values are substituted directly (no quote escaping; if
the orchestrator-supplied REASON contains a literal `"`, it
appears verbatim in the resolution line — only the
`--mark-retro-changes` row of the table below wraps `<REASON>` in
quotes — and may break naive parsers. DETAILS is not subject to
this warning: `--mark-address-failed` writes nothing to stdout, so
DETAILS appears only in the stderr prompt body of `HandoffHuman:
TestsFailedTriage` (which the binary does not double-quote).

| Trigger flag            | Template                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| :---------------------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--advance-level`       | `advanced level: <LEVEL> -> <NEXT_LEVEL>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `--advance-level`       | `at ladder edge (xhigh); no advance` _(when current is already at xhigh — the top of the reasoning ladder. The level token is always literal `xhigh` in this branch. "ladder edge" rather than "ceiling" so the message does not collide with the user's `--ceiling`.)_                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `--drop-level`          | `dropped level: <LEVEL> -> <NEXT_LEVEL>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `--drop-level`          | `at floor (<LEVEL>); no drop` _(when current is already at floor)_                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `--restart-from-floor`  | `restarted from floor: <LEVEL> -> <FLOOR>` _(no separate at-floor row — when current is already at floor the same template renders with `<LEVEL>` and `<FLOOR>` resolving to the same token, e.g. `restarted from floor: low -> low`. The line is always emitted; the at-floor no-op is not silent.)_                                                                                                                                                                                                                                                                                                                                                                                       |
| `--mark-retro-clean`    | `retrospective clean at ceiling (<CEILING>); fixed point reached` _(at configured ceiling — paired with DoneFixedPoint exit)_                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `--mark-retro-clean`    | `retrospective clean at <LEVEL>; advanced to <NEXT_LEVEL>` _(when `current != configured ceiling` and `current < xhigh` — covers both the normal below-ceiling case AND the out-of-band above-ceiling-but-below-xhigh case where `--advance-level` pushed past `--ceiling`. Outcome: `Idle` exit 1.)_                                                                                                                                                                                                                                                                                                                                                                                       |
| `--mark-retro-clean`    | `retrospective clean at <LEVEL>; ladder edge xhigh reached, no advance` _(when current is at xhigh and `current != configured ceiling` — reachable when out-of-band `--advance-level` pushed current up to xhigh past the configured ceiling, OR when prior `--mark-retro-clean` calls walked one rung at a time from above-ceiling to xhigh. Outcome: `Idle` exit 1.)_                                                                                                                                                                                                                                                                                                                     |
| `--mark-retro-changes`  | `retrospective surfaced changes at <LEVEL> ("<REASON>"); restarted from floor: <LEVEL> -> <FLOOR>` _(no separate at-floor row — when current is already at floor the same template renders with both `<LEVEL>` slots and `<FLOOR>` resolving to the same token, e.g. `retrospective surfaced changes at low ("..."); restarted from floor: low -> low`. The line is always emitted; the at-floor no-op is not silent. Both `<LEVEL>` slots resolve to the pre-restart current_level by the placeholder definition. The trailing "restarted from floor: <LEVEL> -> <FLOOR>" segment matches the `--restart-from-floor` template above so a single parser handles both flags' restart-tail.)_ |
| `--mark-address-passed` | `address passed at <LEVEL>; dropped to <NEXT_LEVEL>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `--mark-address-passed` | `address passed at floor <LEVEL>; no drop` _(at configured floor; level is unchanged)_                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |

These resolution lines are advisory diagnostics; orchestrators
should dispatch on `$?` plus the stderr header, not parse stdout.

### Stderr example (HandoffAgent)

```
HandoffAgent: AddressBatch
  prompt: Verify and address 2 review(s) with issues at level low. For each issue: real bug -> fix; false positive -> clarify code; design tradeoff -> document rationale. Then run tests.
```

The header line is fixed (`HandoffAgent: <ActionKind>`); the
`  prompt:` line is the entire prompt template emitted by the
binary (above is the literal `AddressBatch` template with `<COUNT>`
and `<LEVEL>` substituted). Today it is one long line; orchestrators
should still read it as an indented block (every continuation line
that remains indented past column 2 belongs to the prompt) so they
tolerate multi-line prompts if a future ActionKind needs them.

The `Retrospective` prompt template is emitted analogously
(`HandoffAgent: Retrospective` plus a `  prompt: ...` block); both
templates are owned by the binary's `decide` layer and are not
parameterizable from the CLI today.

### Stream ordering

For side-effect invocations that emit a resolution line (the six
non-`--mark-address-failed` flags), the binary writes the stdout
resolution line first, then the stderr Outcome header, then exits
— within the binary's own code path the order is fixed.
`--mark-address-failed` writes nothing to stdout, so this
ordering is trivially satisfied.

stdout and stderr are separate file descriptors with independent
buffering: when an orchestrator captures both streams, OS-level
interleaving means the bytes may arrive at the orchestrator in
either order. Orchestrators should read both streams to completion
(they are guaranteed to land before process exit) and not depend
on byte-level interleaving.

## Filesystem layout

Owned by the domain-neutral `ooda-state` crate. Each invocation
creates one `runs/<run-id>/` subtree; no on-disk state is shared
across invocations and there is no resume protocol.

```
<state-root>/
  runs/
    <run-id>/
      events.jsonl            ← source of truth (append-only typed events)
      blobs/<sha>.<ext>       ← content-addressed payloads (orient
                                snapshots, handoff prompt bodies)
      scratch/                ← per-run codex review subprocess scratch
        <L>-1.log             ← spawned by act; scanned by observe
        <L>-1.exit
        <L>-2.log
        <L>-2.exit
        ...
        <L>-<N>.log
        <L>-<N>.exit
  live/<run-id>               ← empty marker; presence = "active"
```

- `<state-root>` defaults to `$OODA_STATE_HOME` → `$XDG_STATE_HOME/ooda`
  → `$HOME/.local/state/ooda` → `$TMPDIR/ooda`; override with
  `--state-root PATH`. Shared with every OODA-family binary
  (domain identity lives in events, not the path).
- `<run-id>` is an opaque sortable identifier
  (`<YYYYMMDDTHHMMSSZ>-<nanos>-p<pid>`); orchestrators do not
  construct or interpret it.
- `events.jsonl` is the audit-trail source of truth. The first
  event is always
  `{"kind":"run_started","domain":"codex-review","target":{"mode":...,"value":...,"floor":...,"ceiling":...}}`.
  The terminal event is one of `run_halted` / `run_stalled` /
  `run_cap_reached`; presence of the terminal event AND absence
  of the `live/<run-id>` marker together mean the run is sealed.
- `<L>` is `low | medium | high | xhigh`. `<N>` is the configured
  `-n` (parallel review count). Each spawned reviewer writes a
  `<L>-<slot>.log` and a `<L>-<slot>.exit` file in `scratch/`;
  observe scans these to derive the batch state. Nonzero exit
  statuses and zero exits without a non-empty `^codex$` verdict
  block surface as `BinaryError`.
- Orient snapshots and handoff prompt bodies go through
  `blobs/<sha>.<ext>`; events reference them by `BlobRef`.

## Orchestration recipe

This pseudocode is the canonical loop. It captures every exit code
the binary can currently produce, including the codes returned by
`--mark-*` follow-up calls. Every `break` exits the outer
`loop forever`.

**Out-of-band state precondition.** The recipe assumes the
orchestrator has not invoked `--advance-level` outside the loop. If
it has, `current_level` may be strictly above the configured
ceiling, and per the Outcomes table loop mode will keep emitting
`HandoffAgent: Retrospective` (since `current_level != configured
ceiling`) — `--mark-retro-clean` from above-ceiling advances
further toward xhigh rather than back toward the ceiling, so the
canonical recipe will never reach `DoneFixedPoint` from such a
state. Recovery: invoke `--restart-from-floor` once before
re-entering the loop to return `current_level` to the configured
floor. (`--mark-retro-changes` also resets to the floor and would
recover from above-ceiling state, but it additionally records a
`RetrospectiveChanges` outcome with a REASON string — use it only
when an actual retrospective surfaced changes; for plain recovery
from out-of-band overshoot, `--restart-from-floor` is the right
primitive because it carries no recording side effect.)

```
# Recipe-local shell variables. Distinct from the angle-bracket
# placeholders (<LEVEL>, <CEILING>, ...) used in the binary's
# resolution-line and stderr-header templates: those refer to
# the values the binary computes at write time, not to
# these caller-side argv tokens.
#
# Orchestrator-supplied helpers used below (signatures only — the
# orchestrator implements the bodies):
#   dispatch_address_agent(prompt: str)        -> none  # runs the address agent on the prompt
#   dispatch_retro_agent(prompt: str)          -> patterns  # returns iterable of retrospective patterns
#   summarize_patterns(patterns)               -> str  # returns the REASON string fed to --mark-retro-changes
#   implement(patterns)                        -> none  # applies each pattern
#   run_project_tests()                        -> { result: PASS | FAIL, summary: str }
#   surface(stderr: str)                       -> none  # logs the stderr blob to a human
#   surface_to_human(kind, prompt, stderr)     -> none  # halts and surfaces the structured prompt
#   surface_unexpected(exit: int, stderr: str) -> none  # logs an unexpected exit code as fatal
#   shell_quote(s: str) -> str                 # POSIX-shell-safe quoting for `s` (so REASON or
#                                              # DETAILS strings with spaces, quotes, or shell
#                                              # metacharacters survive concatenation into the argv string)
#   expect_or_binary_error(exit, stderr, expected: list[int]) -> bool
#                                                           # Branches in this priority order:
#                                                           # (1) if exit is in `expected`, returns
#                                                           # true (caller then branches on the
#                                                           # actual exit value if multiple codes
#                                                           # were expected); (2) else if exit == 6
#                                                           # (BinaryError), calls surface(stderr)
#                                                           # and returns false; (3) else, calls
#                                                           # surface_unexpected(exit, stderr) and
#                                                           # returns false. The expected list MUST
#                                                           # NOT contain 6 — pass only the canonical
#                                                           # success codes; BinaryError tolerance is
#                                                           # encoded by branch (2). Encapsulates
#                                                           # the class-of-issue rule that every
#                                                           # side-effect call MUST tolerate exit 70
#                                                           # (BinaryError) since the state-tree
#                                                           # open/append can fail on any call.
#                                                           # Caller convention: `if not
#                                                           # expect_or_binary_error(...): break`
#                                                           # — the surface call inside the helper
#                                                           # has already logged the failure, so
#                                                           # the outer loop just breaks. After a
#                                                           # true return the caller dispatches on
#                                                           # the exit value (no further BinaryError
#                                                           # check is needed; that case already
#                                                           # broke out).
# `test_summary` and `reason` below are local strings the orchestrator
# derives — `test_summary` from `run_project_tests().summary`,
# `reason` from a digest of the `patterns` returned by
# `dispatch_retro_agent`. Both flow into `--mark-address-failed
# DETAILS` and `--mark-retro-changes REASON` respectively.
MODE_ARG    = "--uncommitted"          # or --base <BRANCH> / --commit <SHA> / --pr <NUM>
                                       # --pr requires gh and reviews the current worktree
                                       # against the PR's resolved base branch.
FLOOR_ARG   = "--level low"            # the configured-floor flag+value; pass the same
                                       # value to every invocation so side-effect calls
                                       # hit the same resume key
CEILING_ARG = "--ceiling xhigh"        # propagate to side-effect calls so --mark-retro-clean
                                       # uses the same DoneFixedPoint check the loop used.
                                       # If you accept the default, omit (default is xhigh).
ARGS = MODE_ARG + " " + FLOOR_ARG + " " + CEILING_ARG

loop forever:
  exit, stderr := run("ooda-codex-review " + ARGS)

  switch exit:

    0:  # DoneFixedPoint — ceiling-level all-clean reached in loop mode
      break

    5:  # HandoffAgent — read stderr action kind to branch.
        # All stderr-parsing helpers take the stderr stream as their
        # first argument so the binding is explicit when multiple
        # captures are in scope (e.g. inner --mark-* calls below).
        # `first_line(s)` returns the first line of `s`, stripped of
        # its trailing newline (so `first_line("HandoffAgent: AddressBatch\n...")`
        # returns the bare string `"HandoffAgent: AddressBatch"` and the
        # subsequent `removeprefix` / case comparison sees no `\n`).
        # `indented_block_after(s, prefix)` returns the contiguous
        # block starting with `prefix` plus any subsequent lines that
        # remain indented under it (single-line today; multi-line
        # tolerated for future prompts).
        # stderr first line is exactly "HandoffAgent: <ActionKind>"
        # (ActionKind is one of AddressBatch, Retrospective).
      kind := first_line(stderr).removeprefix("HandoffAgent: ")
      prompt := indented_block_after(stderr, "  prompt: ")

      switch kind:

        "AddressBatch":
          dispatch_address_agent(prompt)
          test_result := run_project_tests()
          if test_result.result == PASS:
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-address-passed")
            # Expected: 7 (Idle). The helper handles exit 70 (BinaryError)
            # and any other unexpected code by surfacing and returning false;
            # the orchestrator just breaks if the helper failed.
            if not expect_or_binary_error(m_exit, m_stderr, expected=[7]): break
            continue  # m_exit == 7 (Idle): explicit re-enter the outer loop
          else:
            test_summary := test_result.summary
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-address-failed " + shell_quote(test_summary))
            # Expected: 3 (HandoffHuman). Pass m_stderr explicitly — the
            # TestsFailedTriage prompt lives in the side-effect call's
            # stderr, not the outer AddressBatch stderr.
            if not expect_or_binary_error(m_exit, m_stderr, expected=[3]): break
            surface_to_human("TestsFailedTriage",
                             indented_block_after(m_stderr, "  prompt: "),
                             m_stderr)
            break

        "Retrospective":
          patterns := dispatch_retro_agent(prompt)
          if patterns.is_empty:
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-retro-clean")
            # Expected: 0 (DoneFixedPoint at ceiling) OR 7 (Idle off-ceiling)
            # — two valid exits, so pass both to the helper. After a true
            # return, dispatch on the actual m_exit value.
            if not expect_or_binary_error(m_exit, m_stderr, expected=[0, 7]): break
            if m_exit == 0: break  # DoneFixedPoint, exit outer loop
            continue  # m_exit == 7 (Idle): re-enter the outer loop
          else:
            implement(patterns)
            reason := summarize_patterns(patterns)  # orchestrator-defined digest
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-retro-changes " + shell_quote(reason))
            # Expected: 7 (Idle).
            if not expect_or_binary_error(m_exit, m_stderr, expected=[7]): break
            continue  # m_exit == 7 (Idle): explicit re-enter the outer loop

    3:  # HandoffHuman. Read the action kind from the stderr header
        # (format: "HandoffHuman: <ActionKind>") and the prompt from
        # the indented "  prompt: " line. Today the only emitted kind
        # is TestsFailedTriage (from --mark-address-failed), and the
        # canonical recipe handles --mark-address-failed inline in
        # case 5 (the inner break exits the outer loop). This
        # top-level case 3 is therefore unreachable from the
        # canonical recipe — kept defensively for orchestrators that
        # extend the recipe to call --mark-address-failed at the top
        # of the outer loop, and as a forward guard if a future
        # HandoffHuman producer is added.
      kind := first_line(stderr).removeprefix("HandoffHuman: ")
      prompt := indented_block_after(stderr, "  prompt: ")
      surface_to_human(kind, prompt, stderr); break

    1, 2:  # StuckRepeated or StuckCapReached
      surface(stderr); break

    6:  # BinaryError (e.g. codex spawn failed)
      surface(stderr); break

    7:  # Idle from loop mode — decide had nothing to do this iteration.
        # Continue (next observe will pick up new state).
      continue

    64:  # UsageError — orchestrator passed a malformed argv; programmer
         # error, surface and stop.
      surface(stderr); break

    default:  # exit 2 (WouldAdvance) and 8 (DoneAborted) are reserved
              # today; treat any other code as fatal.
      surface_unexpected(exit, stderr); break
```

The binary owns the state transitions; the orchestrator dispatches
on exit code and reports outcomes back via `--mark-*`. Considering
just the four `--mark-*` flags (a subset of the seven side-effect
flags from the three-class taxonomy above), they fall into two
groups by behavior:

- **Recording marks** (`--mark-retro-clean`, `--mark-retro-changes`,
  `--mark-address-passed`) atomically record a per-level outcome
  AND apply the appropriate ladder change (advance, drop, restart,
  or — for `--mark-retro-clean` at the configured ceiling — terminal
  halt with no ladder change). All three exit `Idle` in every case
  except one: `--mark-retro-clean` exits `DoneFixedPoint` strictly
  when `current_level == configured ceiling`. Above-ceiling
  out-of-band states still exit `Idle` (see the four-row dispatch
  table under `--mark-retro-clean` for the partition).
- **Escalation mark** (`--mark-address-failed`) records nothing and
  does not transition; it only emits `HandoffHuman` so the
  orchestrator can surface the failure with a structured prompt.

The remaining three side-effect flags are the **low-level
primitives** `--advance-level`, `--drop-level`, and
`--restart-from-floor`. They are provided for tests and direct
manipulation. They record the requested transition as an event
against the value passed via `--level` but never emit
`DoneFixedPoint` (even when called at the configured ceiling).
Production orchestrators should prefer `--mark-*`.

## State model

The binary uses the domain-neutral `ooda-state` crate (see
Filesystem layout above). One invocation = one run; there is no
resume protocol and no cross-invocation manifest. Ladder position
is the orchestrator's responsibility — pass it in via `--level`
each time; the binary records what arrived and what it computed.

## What this binary does NOT do

- It does not orchestrate the address agent. That's the outer
  Claude session.
- It does not decide _whether_ to address an issue — every flagged
  verdict triggers an `AddressBatch` halt. The agent verifies and
  classifies (real bug / false positive / design tradeoff) and
  always produces a code change.
- It does not run tests. The orchestrator runs tests after
  `AddressBatch` and reports the outcome via
  `--mark-address-passed` or `--mark-address-failed`.
- It does not checkout PR branches. `--pr NUM` only resolves the
  PR's base branch and reviews the current worktree against it.
- It does not synthesize the retrospective. The orchestrator
  dispatches the agent and reports back via
  `--mark-retro-clean` or `--mark-retro-changes`.

It DOES own the ladder transitions: each Recording-mark invocation
(`--mark-retro-clean`, `--mark-retro-changes`, `--mark-address-passed`)
records the outcome and applies the right transition (advance,
drop, restart-from-floor) atomically. The Escalation mark
`--mark-address-failed` is the exception — it neither records nor
transitions, only emits `HandoffHuman` to surface the failure.

## Reasoning ladder

The default ladder is `low → medium → high → xhigh` (totally
ordered: `low < medium < high < xhigh`). The floor is `--level`
(default `low`); the ceiling is `--ceiling` (default `xhigh`).
Both defaults live in this binary's CLI parser. The loop walks
within `[floor, ceiling]`; drops are clamped at the floor and
restarts reset to it.

Retrospective handoff fires from loop mode at every per-level
all-clean where `current_level != configured ceiling` (typically
**below** the ceiling — the normal climbing path — but also when
out-of-band `--advance-level` has pushed current strictly above
the ceiling). At the configured ceiling exactly, all-clean halts
directly with `DoneFixedPoint` (exit 0) — loop mode emits no
ceiling-level Retrospective handoff. If the orchestrator runs its
own ceiling-level retrospective out of band, it records the result
via `--mark-retro-clean` (which records the per-level `Clean`
outcome and yields `DoneFixedPoint` per Row 2 of the dispatch
table); this is a recording call, not a Retrospective handoff
emission.

A "full fixed point" therefore means: every level from floor to
ceiling has reached all-clean, and (for sub-ceiling levels) the
orchestrator's retrospective produced no architectural changes (i.e.
each was reported via `--mark-retro-clean`, not
`--mark-retro-changes`).

## Examples

> **Ladder-position reminder.** Each invocation is independent.
> Side-effect calls compute their transition against the `--level`
> the orchestrator passes — repeat the level the loop invocation
> used (or the post-transition value from the prior side-effect's
> resolution line). `--ceiling` should also be re-passed so the
> binary's ladder-edge checks line up; mismatching it only affects
> the single invocation that drifted, since no state crosses
> invocations.

```bash
# Default loop: review uncommitted changes, floor=low, ceiling=xhigh,
# 3 reviewers per level
ooda-codex-review --uncommitted

# Loop: current branch vs master, start at medium, climb only to high
ooda-codex-review --base master --level medium --ceiling high -n 5

# Loop: a specific PR, max 20 iterations. Requires gh; reviews the
# current worktree against the PR's resolved base branch.
ooda-codex-review --pr 1234 --max-iter 20

# Side-effect: orchestrator reports tests passed after AddressBatch
ooda-codex-review --uncommitted --level low --mark-address-passed

# Side-effect: orchestrator reports tests failed (will exit
# HandoffHuman / 3 with DETAILS embedded in the prompt)
ooda-codex-review --uncommitted --level low --mark-address-failed "test_X failed at line 42: expected ok, got Err(...)"

# Side-effect: orchestrator reports retrospective clean
# (DoneFixedPoint when --level == --ceiling exactly; Idle otherwise)
ooda-codex-review --uncommitted --level xhigh --ceiling xhigh --mark-retro-clean

# Side-effect: orchestrator implemented retrospective patterns
ooda-codex-review --uncommitted --level medium --mark-retro-changes "Found N+1 pattern"

# Side-effect chained to the second loop above: repeat --base master
# AND --level medium AND --ceiling high so the binary's edge checks
# line up with the loop invocation's view of the ladder.
ooda-codex-review --base master --level medium --ceiling high --mark-address-passed
```
