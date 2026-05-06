---
name: ooda-codex-review
description: Drive `codex review` to fixed point across the reasoning ladder. Two invocation modes — loop (observe → orient → decide → optionally act → emit one Outcome) and side-effect (apply at most one recorder mutation, then emit one Outcome; some side-effect flags only emit and do not mutate). The orchestrator dispatches on the exit code; for `HandoffAgent` (exit 5) and `HandoffHuman` (exit 3) it also reads the action name from the stderr header to choose a branch.
args:
  - name: --uncommitted
    description: Mode flag. Review working-tree changes vs HEAD. The four mode flags (`--uncommitted`, `--base`, `--commit`, `--pr`) are mutually exclusive; exactly one is required for any non-help invocation (loop or side-effect). `--help` / `-h` is the sole exception — it requires no mode flag and short-circuits before any other validation. Passing two or more mode flags raises `UsageError`.
  - name: --base BRANCH
    description: Mode flag. Review the current branch vs BRANCH.
  - name: --commit SHA
    description: Mode flag. Review one commit by 40-hex SHA.
  - name: --pr NUM
    description: Mode flag. Review a PR's changes by number. The recorder key remains `pr/NUM`, but loop mode resolves the PR's base branch with `gh pr view NUM --json baseRefName --jq .baseRefName` and invokes the current `codex review` CLI as `codex review --base <baseRefName>`. The caller must already be in the intended PR worktree/branch; the binary does not checkout or mutate branches.
  - name: --level LVL
    description: 'Configured floor for the run. One of low|medium|high|xhigh. Default low. Persisted in the manifest as the field `start_level` (see "Canonical names for the configured floor" below for the three-name correspondence). Part of the resume key `(target, start_level)`: on later invocations the recorder resumes only when this value matches; otherwise it starts a new run (no error).'
  - name: --ceiling LVL
    description: User-configured upper bound of the climb (distinct from the ladder edge xhigh, which is the absolute top of the reasoning ladder). One of low|medium|high|xhigh (same token set as `--level`). All-clean at this level halts `DoneFixedPoint` directly without a Retrospective handoff. Default xhigh, set by the binary's CLI parser. Must be >= --level (UsageError otherwise). Levels are totally ordered low < medium < high < xhigh.
  - name: -n N
    description: Parallel review count. Default 3, must be ≥1. Recorded in the manifest as `batch_size`. Not part of the resume key.
  - name: --max-iter N
    description: Loop-iteration cap. Default 50, must be ≥1. Silently ignored by side-effect invocations (NOT a UsageError — `--max-iter` is one of the loop-mode-only knobs that has no recorder side effect; see Side-effect mode below for the full list).
  - name: --state-root PATH
    description: Directory for batch logs and recorder state. Default $TMPDIR/ooda-codex-review (with `$TMPDIR` resolved by Rust's `std::env::temp_dir`, which falls back to `/tmp` on Unix when the env var is unset).
  - name: --codex-bin PATH
    description: Path to the `codex` binary. Default `codex` (PATH lookup).
  - name: --criteria STRING
    description: "Reserved but currently unsupported. The current `codex review` CLI rejects positional prompts when combined with target modes (`--uncommitted`, `--base`, `--commit`), so this binary fails fast with `UsageError` whenever `--criteria` is passed. Omit it and use codex's built-in review criteria."
  - name: --fresh
    description: Ignore the `latest` pointer; force a new run. Loop-mode only. Combining with any side-effect flag is a UsageError (the side-effect would mutate a brand-new manifest with no review history; semantics are not defined).
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
      | strictly below configured ceiling               | advance one rung up the ladder (toward the ceiling) | `Idle` (exit 7)         |
      | == configured ceiling                           | no level change; halt terminal-success            | `DoneFixedPoint` (exit 0) |
      | strictly above configured ceiling AND < xhigh   | advance one rung up the ladder (further above the ceiling, NOT back toward it) | `Idle` (exit 7) |
      | == xhigh AND `current != configured ceiling`    | no level change (ladder edge no-op)               | `Idle` (exit 7)           |

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
    description: "Side-effect. Orchestrator reports the retrospective surfaced architectural changes (the orchestrator should implement those changes BEFORE invoking this flag — the flag closes the level out and resets `current_level` to the configured floor, it does not gate the implementation). Records the per-level outcome (variant `RetrospectiveChanges`) with REASON, then sets `current_level` to the configured floor (when `current_level` is already at the floor this is a no-op transition but the outcome still records — same shape as `--restart-from-floor`'s at-floor render). Above-ceiling state (out-of-band, e.g. after `--advance-level` overshoot) is handled identically: the reset always lands at the configured floor regardless of pre-mutation level, and the resolution-line `<LEVEL>` slots reflect the pre-mutation value. Outcome: `Idle` (exit 7)."
  - name: --mark-address-passed
    description: "Side-effect. Orchestrator reports the address agent fixed the batch and the project's tests passed. Behavior every invocation: (1) record the per-level outcome (variant `Addressed`) with a `review(s) with issues` count (see `<COUNT>` placeholder); (2) drop one rung, clamped at the configured floor; when already at the floor, keep the level but move to the next unused batch number so the addressed logs are never reread; (3) exit `Idle` (exit 7) regardless of clamp. The recorded count is the number of reviewers in the current batch whose on-disk verdict classified as has-issues, derived via the same verdict-classify pass observe uses (the `scan_batch` heuristic in the architecture table below); the orchestrator has not modified those logs, so this re-classifies the same logs the AddressBatch handoff was based on (pre-fix count of reviews-with-issues, not the post-fix count which would be zero). On the canonical path the count is always >= 1 because the AddressBatch handoff that preceded this call only fires when at least one reviewer flagged issues. Off-path behavior (when no batch is present at the current level — orchestrator should only invoke `--mark-address-passed` after an `AddressBatch` handoff): the same three-step behavior runs, with `Addressed(0)` recorded; no error path, no UsageError. Above-ceiling state (out-of-band, e.g. after `--advance-level` overshoot) is handled identically: the drop step always moves one rung down regardless of position relative to the ceiling, clamped at the configured floor. Rationale (per the loop-codex-review meta-procedure this binary implements): drop one level after a passing fix so the next iteration re-runs reviews at lower reasoning to confirm the patch passes simpler scrutiny too."
  - name: --mark-address-failed DETAILS
    description: "Side-effect. Orchestrator reports post-address tests failed. Does NOT record any per-level outcome and does NOT transition the level — purely an escalation. Outcome: `HandoffHuman` (exit 3). The stderr action kind on the header line is `TestsFailedTriage`. The prompt is a fixed template that interpolates the manifest's current_level (the level the addressed batch ran at) and DETAILS verbatim: `Tests failed after addressing review batch at level <LEVEL>. Surface to a human for triage. Details: <DETAILS>`. Unlike the recording marks, this side-effect writes nothing to stdout."
  - name: --advance-level
    description: "Side-effect (low-level primitive). Bump manifest current_level by one rung along the full reasoning ladder (low<medium<high<xhigh). Bounded by the **ladder edge xhigh**, NOT the configured ceiling — so when configured ceiling < xhigh it can push current_level above the configured ceiling. To return to a clean state from out-of-band overshoot the orchestrator should call `--restart-from-floor` (a single `--mark-retro-clean` only advances one more rung at most and never emits DoneFixedPoint while above the configured ceiling). Outcome `Idle` (exit 7) on success and on ladder-top no-op. Prefer `--mark-retro-clean`; this primitive is provided for tests and direct manipulation and never emits `DoneFixedPoint` itself."
  - name: --drop-level
    description: "Side-effect (low-level primitive). Drop one rung, clamped at the configured floor (`--level`). Outcome `Idle` (exit 7) on success and on floor no-op. Prefer `--mark-address-passed` for the canonical post-fix flow (which drops one rung AND records an `Addressed` outcome with the reviewer-with-issues count); use `--drop-level` only when no address-batch context applies."
  - name: --restart-from-floor
    description: "Side-effect (low-level primitive). Reset current_level to the configured floor. Outcome `Idle` (exit 7) on success and on at-floor no-op (current_level already == floor). The flag name describes the destination: it restarts the climb starting from the floor. The resolution line is always emitted (the at-floor no-op is not silent); when current is already at floor it renders e.g. as `restarted from floor: low -> low` — the same template, with `<LEVEL>` and `<FLOOR>` resolving to the same token. Prefer `--mark-retro-changes` for the canonical retrospective-changes flow (which also restarts from the floor and additionally records a per-level outcome with REASON); use `--restart-from-floor` only when no retrospective context applies (tests, recovery from out-of-band overshoot)."
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

## Two invocation modes

A single binary serves two distinct flows; both produce one `Outcome`
with one exit code.

- **Loop mode** — the default when no side-effect flag is set. The
  binary internally repeats observe → orient → decide → optionally
  act, up to `--max-iter` iterations, until any single iteration
  produces an Outcome that ends the invocation. Decide returning no
  candidate action ends the invocation immediately with the `Idle`
  Outcome (exit 7) — the binary does NOT keep iterating internally
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
  Skips the OODA loop entirely; opens the recorder, optionally
  mutates the manifest, and emits the documented Outcome. The
  seven side-effect flags split into three classes by their
  effect on manifest state:

  | Class                     | Flags                                                                 | Records outcome? |                          Changes `current_level`?                           |
  | ------------------------- | --------------------------------------------------------------------- | :--------------: | :-------------------------------------------------------------------------: |
  | **Recording marks**       | `--mark-retro-clean`, `--mark-retro-changes`, `--mark-address-passed` |      always      | usually (see per-flag descriptions for ceiling- and floor-edge no-op cases) |
  | **Transition primitives** | `--advance-level`, `--drop-level`, `--restart-from-floor`             |      never       |                   usually (no-op at ladder edge / floor)                    |
  | **Escalation**            | `--mark-address-failed`                                               |      never       |                       never (purely surfaces a halt)                        |

  Side-effect flags are mutually exclusive with each other and
  with `--fresh` (UsageError if combined). Loop-mode-only knobs
  that have no recorder effect are silently parsed and ignored
  alongside a side-effect flag only when they are otherwise valid;
  today this means `--max-iter` (purely the loop iteration cap).
  `--criteria` is not valid in any mode until the current `codex
review` CLI supports prompts with target modes. All non-help
  invocations require a mode flag for context (the recorder resume
  key is `(target, start_level)`; see Resume semantics).

  **BinaryError tolerance.** Every side-effect call documents an
  expected non-error Outcome (Idle, DoneFixedPoint, or HandoffHuman),
  but the recorder open/record path can also surface `BinaryError`
  (exit 6) on any side-effect invocation regardless of flag — see the
  Outcomes table for the trigger sources. The per-flag descriptions
  below name only the canonical-path Outcome to keep them focused;
  orchestrators MUST treat exit 6 as a possible additional outcome
  for every `--mark-*` and primitive call. The orchestration recipe
  encapsulates this in the `expect_or_binary_error` helper.

  **Per-level outcome variants** recorded by the three Recording
  marks (each `--mark-*` records exactly one variant on the
  current level before any transition; the Escalation mark and the
  Transition primitives record nothing):

  | `--mark-*` flag         | Recorded variant       | Notes                                                                                  |
  | :---------------------- | :--------------------- | :------------------------------------------------------------------------------------- |
  | `--mark-retro-clean`    | `Clean`                | Pairs with the Retrospective handoff "no architectural changes" path; recorded always. |
  | `--mark-retro-changes`  | `RetrospectiveChanges` | Carries the orchestrator's REASON string.                                              |
  | `--mark-address-passed` | `Addressed`            | Carries the pre-fix `review(s) with issues` count (see `<COUNT>`).                     |

  The asymmetric variant names (`Clean` vs `RetrospectiveChanges`
  vs `Addressed`) reflect the recorder's existing on-disk naming;
  they are not intended to share a prefix.

**Canonical names for the configured floor.** Three names denote
the same value: the CLI flag `--level`, the manifest field
`start_level`, and the prose concept "configured floor" (used
mainly when contrasting with the configured ceiling). The
canonical surface depends on context — flag descriptions use
`--level`, resume-key tables use `start_level`, and ladder prose
uses "configured floor" — but they always denote the same value.

The **configured floor** (`--level` / `start_level`) is part of
the resume key, so side-effect invocations must pass the same
`--level` that the loop invocation used; otherwise the recorder
will treat them as a different ladder and start a fresh run with
no ladder history. If `--level` is omitted, the resolved floor is
`low` — the default — which must also match.

The **configured ceiling** (`--ceiling`, default `xhigh`) is NOT
part of the resume key. The ceiling lives in the LoopConfig the
binary builds at parse time and is not persisted to the manifest;
side-effect invocations should re-pass the same `--ceiling` they
used in loop mode if the orchestrator wants the binary's halt
checks to use the same value, but a mismatch only affects the
**current** invocation's halt logic, not stored state.

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

| Step                                                                                                                                                                                                         | Procedural (this binary)                                                                                                                                                                                                                         | LLM (orchestrator)                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Discover repo root                                                                                                                                                                                           | `git rev-parse --show-toplevel`                                                                                                                                                                                                                  |                                                                                                                                                              |
| Spawn n codex reviews                                                                                                                                                                                        | `RunReviews` (Full) writes `<LEVEL>-<slot>.log` synchronously, spawns codex through a wrapper, and records `<LEVEL>-<slot>.exit` when the child finishes                                                                                         |                                                                                                                                                              |
| Poll log/exit files for marker                                                                                                                                                                               | `AwaitReviews` (Wait; 30s default)                                                                                                                                                                                                               |                                                                                                                                                              |
| Extract verdict + classify                                                                                                                                                                                   | `scan_batch` heuristic                                                                                                                                                                                                                           |                                                                                                                                                              |
| Verify + address batch, then run tests (test execution is orchestrator-owned; the binary itself never spawns tests)                                                                                          | binary emits the `HandoffAgent: AddressBatch` Outcome (header on stderr, then a one-line prompt body with `<COUNT>` and `<LEVEL>` substituted; the prompt body says e.g. "Verify and address 2 review(s) with issues at level low...") and exits | `HandoffAgent: AddressBatch` → orchestrator runs the agent + tests, reports via `--mark-address-passed` or `--mark-address-failed`                           |
| Synthesize retrospective (and, if the agent surfaces patterns, the orchestrator implements them BEFORE invoking `--mark-retro-changes` — the flag closes the level out, it does not gate the implementation) | binary emits the `HandoffAgent: Retrospective` Outcome — header on stderr followed by the prompt template — and exits                                                                                                                            | `HandoffAgent: Retrospective` → orchestrator runs the agent, reports via `--mark-retro-clean` (no patterns) or `--mark-retro-changes` (patterns implemented) |
| Climb / drop / restart                                                                                                                                                                                       | recorder mutation inside the recording marks (`--mark-retro-clean` / `--mark-retro-changes` / `--mark-address-passed`) and the low-level primitives (`--advance-level` / `--drop-level` / `--restart-from-floor`)                                |                                                                                                                                                              |

The poll cadence is 30s by default; the `OODA_AWAIT_SECS` env var
overrides it (intended for tests; orchestrators should leave it
unset in production).

## Outcomes (exit codes)

The boundary contract is **one variant → one exit code**. The
orchestrator dispatches on `$?` for the coarse routing decision; for
`HandoffAgent` (exit 5) and `HandoffHuman` (exit 3) it also reads
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
| 0    | `DoneFixedPoint`  | Loop mode: a per-level batch completes with all reviews clean while `current_level == configured ceiling` exactly (strict equality; an above-ceiling all-clean state — reachable only via out-of-band `--advance-level` — produces `HandoffAgent: Retrospective` instead, see exit 5). Side-effect mode: `--mark-retro-clean` while `current_level == configured ceiling` exactly (per Row 2 of its dispatch table; above-ceiling cases produce `Idle`).                                                                                                                                                     |
| 1    | `StuckRepeated`   | Loop mode: the same `(action kind, blocker key)` pair fired on two consecutive non-Wait iterations (Wait iterations are skipped over when evaluating "consecutive"). Wait actions (e.g. `AwaitReviews` polling) are excluded from this check; cycling on Wait is normal. The stderr header carries the action kind and blocker key of the repeated action. The set of action kinds that can recur this way is narrowed by the architecture: today only `RunReviews` qualifies (handoff actions exit before they could recur, Wait actions are excluded). See the `<ActionKind>` placeholder reference below. |
| 2    | `StuckCapReached` | Loop mode: `--max-iter` iterations elapsed without a halt. The stderr header carries the action kind and blocker key of the **last attempted action** (which may be a Wait, since the cap can fire on a polling iteration).                                                                                                                                                                                                                                                                                                                                                                                  |
| 3    | `HandoffHuman`    | Side-effect: `--mark-address-failed`. (Loop mode never selects a `HandoffHuman` action today; this is the only producer.)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| 4    | `WouldAdvance`    | **Reserved**, not producible. Future inspect-mode (single-pass observe + decide, no act) would map a non-halt decision into this code. There is currently no CLI surface to invoke it.                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 5    | `HandoffAgent`    | Loop mode: a per-level batch completes with at least one review flagging issues → `AddressBatch` (regardless of current level vs. ceiling); a per-level batch completes with all reviews clean while `current_level != configured ceiling` → `Retrospective` (whether current is below OR strictly above ceiling — above-ceiling is reachable via out-of-band `--advance-level`). The stderr header carries the action name (`AddressBatch` or `Retrospective`); the orchestrator reads it to branch.                                                                                                        |
| 6    | `BinaryError`     | Caught external failure. Triggers split by mode: codex spawn failure, nonzero codex exit status, and malformed completed codex logs are loop-mode only (side-effect mode never spawns codex); IO errors and recorder open/record failures are producible from BOTH modes (recorder open happens in every non-help invocation, so any `--mark-*` or low-level primitive can return `BinaryError` if the recorder fails). The header line is `BinaryError: <message>`.                                                                                                                                         |
| 7    | `Idle`            | Loop mode: decide had nothing to do this iteration. Side-effect mode: any of `--advance-level`, `--drop-level`, `--restart-from-floor`, `--mark-retro-clean` (when current is at any level **other than** the configured ceiling — both below AND above), `--mark-retro-changes`, `--mark-address-passed` completing successfully.                                                                                                                                                                                                                                                                           |
| 8    | `DoneAborted`     | **Reserved**, not producible. The variant exists for future SIGINT / abort handling; no trigger is wired today.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| 64   | `UsageError`      | CLI parse / validation failure (unknown flag, invalid value, conflicting flags, `--fresh` combined with a side-effect, etc.).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |

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
| `DoneFixedPoint`  | `DoneFixedPoint`                                                                                                                                  |
| `Idle`            | `Idle`                                                                                                                                            |
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
| `<ActionKind>`  | The decide-layer action name. The set of `<ActionKind>` tokens that can appear in stderr headers today is fixed and small — orchestrators should branch on the values listed here and treat any unrecognized token as opaque. **Producible today:** `HandoffAgent` → `AddressBatch` or `Retrospective`; `HandoffHuman` → `TestsFailedTriage` (only emitted by the `--mark-address-failed` side-effect — never selected by the loop's decide layer); `StuckRepeated` → `RunReviews` (only — see narrowing note below); `StuckCapReached` → either `RunReviews` or the Wait kind `AwaitReviews` (cap can fire on a polling iteration). **Reserved (header format defined but not currently emitted):** `WouldAdvance` reserves an `<ActionKind>` slot but is not currently producible. **Carries no `<ActionKind>`:** `DoneFixedPoint`, `Idle`, `BinaryError`, `UsageError`, `DoneAborted`. **`StuckRepeated` narrowing:** the `StuckRepeated` row in the Outcomes table (exit 1) says it fires on "two consecutive non-Wait iterations of the same `(action kind, blocker key)` pair". The set of decide-layer actions that are non-halting AND non-Wait is exactly `{RunReviews}` today: handoff actions like `AddressBatch` and `Retrospective` are also non-Wait but produce a halt Outcome that exits the invocation, so they cannot recur within a single binary invocation; Wait actions like `AwaitReviews` execute on each iteration but are excluded from the StuckRepeated equality check, so the "two consecutive non-Wait iterations" rule is evaluated by skipping over any interleaved Wait iterations. Hence `StuckRepeated` always carries `RunReviews` today. **Reserved future decide-layer variants (in the `ActionKind` enum, no producer today):** `ParseVerdicts`, `AdvanceLevel`, `DropLevel`, `RestartFromFloor`, `RequestCriteriaRefinement`. The names `AdvanceLevel` / `DropLevel` / `RestartFromFloor` mirror the side-effect CLI flags but are independent enum variants — the side-effect flags themselves are NOT decide-layer actions, they are CLI surfaces handled by side-effect dispatch. **Excluded from future decide-layer use:** the enum also carries a `RunTests` variant for shape-symmetry with the procedural pipeline, but `RunTests` is permanently NOT a decide-layer action — running tests is permanently the orchestrator's job (see "What this binary does NOT do" below). Decide will never emit `RunTests`, and `RunTests` will never appear in any stderr header. |
| `<LEVEL>`       | A reasoning-level token (`low` / `medium` / `high` / `xhigh`). Substituted with the manifest's `current_level` at the moment of invocation. For side-effect resolution lines that mutate `current_level` (`--mark-retro-clean`, `--mark-retro-changes`, `--mark-address-passed`, `--restart-from-floor`, `--advance-level`, `--drop-level`), `<LEVEL>` always reflects the **pre-mutation** value and `<NEXT_LEVEL>` (when present) reflects the **post-mutation** value. Per-template notes never override this convention; they only narrow it.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `<NEXT_LEVEL>`  | The reasoning-level token after a one-rung transition. Direction depends on the template: advance → one rung up; drop → one rung down. The placeholder name is direction-agnostic. Used by the one-rung templates (`--advance-level`, `--drop-level`, `--mark-address-passed`, and `--mark-retro-clean` for its non-edge advance case); restart-style templates substitute `<FLOOR>` directly for their destination slot since the destination is always the configured floor — no `<NEXT_LEVEL>` slot appears in `--restart-from-floor` or `--mark-retro-changes` resolution lines.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `<FLOOR>`       | The configured floor (`--level`'s value, or `low` if `--level` was omitted).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `<CEILING>`     | The configured ceiling (`--ceiling`'s value, or `xhigh` if `--ceiling` was omitted).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `<COUNT>`       | The number of reviewers in the current batch whose on-disk verdict classified as has-issues at the moment of invocation. Each reviewer contributes 0 or 1 to the count regardless of how many distinct issues their verdict listed — so this is a count of "reviews with issues", not a count of distinct issues. Both rendering sites use this same canonical phrasing: the AddressBatch prompt body emits `<COUNT> review(s) with issues` (e.g. `Verify and address 2 review(s) with issues at level low`); the `--mark-address-passed` resolution lines emit `(<COUNT> review(s) with issues)`. The orchestrator does not edit reviewer logs, so this re-reads the same logs the AddressBatch handoff was based on (pre-fix count, not post-fix). `0` only if no batch is present (off-path).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `<BATCH>`       | The next unused batch number at the current level. Used only by the floor-clamped `--mark-address-passed` resolution line, where the level cannot drop but the recorder still advances to a fresh batch directory.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `<REASON>`      | Free-form orchestrator-supplied REASON string passed to `--mark-retro-changes`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `<DETAILS>`     | Free-form orchestrator-supplied DETAILS string passed to `--mark-address-failed`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `<message>`     | Caught-error message embedded in `BinaryError: <message>`. Covers every `BinaryError` trigger source from the Outcomes table: codex spawn failure, codex child exit/log failure, IO error, recorder open failure, recorder record failure.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
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
| `--mark-retro-clean`    | `retrospective clean at <LEVEL>; advanced to <NEXT_LEVEL>` _(when `current != configured ceiling` and `current < xhigh` — covers both the normal below-ceiling case AND the out-of-band above-ceiling-but-below-xhigh case where `--advance-level` pushed past `--ceiling`. Outcome: `Idle` exit 7.)_                                                                                                                                                                                                                                                                                                                                                                                       |
| `--mark-retro-clean`    | `retrospective clean at <LEVEL>; ladder edge xhigh reached, no advance` _(when current is at xhigh and `current != configured ceiling` — reachable when out-of-band `--advance-level` pushed current up to xhigh past the configured ceiling, OR when prior `--mark-retro-clean` calls walked one rung at a time from above-ceiling to xhigh. Outcome: `Idle` exit 7.)_                                                                                                                                                                                                                                                                                                                     |
| `--mark-retro-changes`  | `retrospective surfaced changes at <LEVEL> ("<REASON>"); restarted from floor: <LEVEL> -> <FLOOR>` _(no separate at-floor row — when current is already at floor the same template renders with both `<LEVEL>` slots and `<FLOOR>` resolving to the same token, e.g. `retrospective surfaced changes at low ("..."); restarted from floor: low -> low`. The line is always emitted; the at-floor no-op is not silent. Both `<LEVEL>` slots resolve to the pre-restart current_level by the placeholder definition. The trailing "restarted from floor: <LEVEL> -> <FLOOR>" segment matches the `--restart-from-floor` template above so a single parser handles both flags' restart-tail.)_ |
| `--mark-address-passed` | `address passed at <LEVEL> (<COUNT> review(s) with issues); dropped to <NEXT_LEVEL>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `--mark-address-passed` | `address passed at floor <LEVEL> (<COUNT> review(s) with issues); no drop; advanced to batch <BATCH>` _(at configured floor; level is unchanged, but the recorder moves to the next unused batch number so the addressed batch is not reread)_                                                                                                                                                                                                                                                                                                                                                                                                                                              |

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

```
<state-root>/
  <repo-id>/
    <target-key>/                target_root()
      runs/
        <run-id>/                current_run_dir()
          manifest.json
          levels/
            level-<L>/
              batch-<n>/         batch_dir()  observe + act share this
                <L>-1.log
                <L>-1.exit
                <L>-2.log
                <L>-2.exit
                ...
                <L>-<N>.log
                <L>-<N>.exit
      latest                     pointer file (plain text, not a symlink); contents = <run-id>
```

- `<L>` is `low | medium | high | xhigh` — the per-level subdirectory
  name and the per-log filename prefix both use the same token.
- `<N>` is the configured `-n` (parallel review count); each batch
  contains one log file per spawned reviewer, and each completed
  child writes a matching `.exit` file. Nonzero exit statuses and
  zero exits without a non-empty `^codex$` verdict block surface as
  `BinaryError` instead of polling forever.
- `<repo-id>` = `<repo-basename>-<sha256(remote-url)[..12]>` (or
  `<repo-basename>-noremote` when no remote is configured).
- `<target-key>` = `uncommitted` | `base/<branch>` | `commit/<sha>` |
  `pr/<num>`. Branch names containing `/` (e.g. `feature/x`) are
  written verbatim — the recorder relies on the filesystem to
  handle nested path segments.
- `<run-id>` = `<utc-second>-<nanos>-p<pid>` where `<utc-second>`
  is `YYYYMMDDTHHMMSSZ` (ISO 8601 basic format, second precision; this is the separator-stripped variant of RFC 3339, which requires the dashes and colons),
  `<nanos>` is a 9-digit zero-padded nanosecond field of the same
  instant, and `<pid>` is the process id. Sortable
  lexicographically. Uniqueness comes from the
  `<nanos>`+`<pid>` pair: `<nanos>` separates serial invocations
  on a single process; `<pid>` separates concurrent invocations
  from different processes that happen to share the same
  nanosecond instant. Single-process clock-resolution collisions
  are not defended against by this scheme.
- `latest` is a plain text file containing only the active run-id,
  written without a trailing newline. Tools should `trim()` before
  comparing.

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
# manifest state at the moment the binary writes a line, not to
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
#                                                           # side-effect call MUST tolerate exit 6
#                                                           # (BinaryError) since recorder
#                                                           # open/record can fail on any call.
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
            # Expected: 7 (Idle). The helper handles exit 6 (BinaryError)
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

    default:  # exit 4 (WouldAdvance) and 8 (DoneAborted) are reserved
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
manipulation. They mutate the manifest's `current_level` but do
not record a per-level outcome and never emit `DoneFixedPoint`
(even when run at the configured ceiling). Production
orchestrators should prefer `--mark-*`.

## Resume semantics

By default each invocation tries to resume the prior run named by
`<target_root>/latest`. The recorder accepts the resume only when
the manifest's `(target, start_level)` matches the invocation's
arguments; on any mismatch it silently creates a new run (no error,
no UsageError — just a fresh `<run-id>` directory under the same
`<target-key>`).

The resume vs fresh decision is the recorder's internal state. The
externally visible artifacts are the new `<run-id>` directory under
`runs/` and the updated `latest` pointer; the orchestrator does not
need to act on the resume reason directly.

| Reason             | Trigger                                                                                                                                                                                                                                                                                                         |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Forced             | `--fresh` was passed (loop mode only; `--fresh` + side-effect is a UsageError, not a resume).                                                                                                                                                                                                                   |
| NoLatestPointer    | First invocation for this `(repo-id, target-key)` — no `latest` exists yet. (A change in mode flag, e.g. `--uncommitted` → `--pr 42`, lands here: each target has its own `<target-key>` directory and its own `latest` pointer, so a target change is structurally indistinguishable from a first invocation.) |
| LatestDangling     | `latest` points to a `<run-id>` directory that no longer exists.                                                                                                                                                                                                                                                |
| ManifestUnreadable | `manifest.json` is missing or fails to deserialize.                                                                                                                                                                                                                                                             |
| LevelMismatch      | The manifest's recorded `start_level` differs from the invocation's resolved `--level` (which is the explicit value, or `low` if `--level` was omitted).                                                                                                                                                        |

The resume key is **`(target, start_level)`** — both must match.
`-n` (`batch_size`) is recorded in the manifest but is **not** part
of the resume key; resuming with a different `-n` is allowed (the
existing batch keeps its original size; new batches use the new
`-n`).

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

> **Resume-key reminder.** Side-effect invocations must repeat the
> same `--<mode-flag>` and `--level` (or omit `--level` consistently
> to match the `low` default) used by the loop invocation, otherwise
> the recorder treats them as a different ladder and silently starts
> a fresh run with no review history. `--ceiling` is NOT part of the
> resume key (see "Resume semantics") but should also be re-passed so
> `--mark-retro-clean`'s `DoneFixedPoint` halt check uses the same
> value the loop did; mismatching it only affects that one
> invocation's halt logic, not stored state. The Examples below show
> the loop forms; pair each side-effect call with the matching mode
> flag, floor, and ceiling — see "Resume semantics" for the precise
> key.

```bash
# Default loop: review uncommitted changes, floor=low, ceiling=xhigh,
# 3 reviewers per level
ooda-codex-review --uncommitted

# Loop: current branch vs master, start at medium, climb only to high
ooda-codex-review --base master --level medium --ceiling high -n 5

# Loop: a specific PR, max 20 iterations. Requires gh; reviews the
# current worktree against the PR's resolved base branch.
ooda-codex-review --pr 1234 --max-iter 20

# Force a brand-new run, ignoring the latest pointer
ooda-codex-review --uncommitted --fresh

# Side-effect: orchestrator reports tests passed after AddressBatch
ooda-codex-review --uncommitted --mark-address-passed

# Side-effect: orchestrator reports tests failed (will exit
# HandoffHuman / 3 with DETAILS embedded in the prompt)
ooda-codex-review --uncommitted --mark-address-failed "test_X failed at line 42: expected ok, got Err(...)"

# Side-effect: orchestrator reports retrospective clean
# (DoneFixedPoint when current_level == configured ceiling exactly;
# Idle for every other level — below or above the ceiling — see
# the four-row dispatch table under --mark-retro-clean for the
# full partition)
ooda-codex-review --uncommitted --mark-retro-clean

# Side-effect: orchestrator implemented retrospective patterns
ooda-codex-review --uncommitted --mark-retro-changes "Found N+1 pattern"

# Side-effect chained to the second loop above: repeat --base master
# AND --level medium for the resume key, plus --ceiling high so the
# halt logic uses the same value the loop did (see Resume semantics
# and the Resume-key reminder above).
ooda-codex-review --base master --level medium --ceiling high --mark-address-passed
```
