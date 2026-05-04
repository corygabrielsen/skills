---
name: ooda-codex-review
description: Drive `codex review` to fixed point across the reasoning ladder. Two invocation modes — loop (observe → orient → decide → optionally act → emit one Outcome) and side-effect (apply one recorder mutation → emit one Outcome). The orchestrator dispatches on the exit code; for `HandoffAgent` (exit 5) and `HandoffHuman` (exit 3) it also reads the action name from the stderr header to choose a branch.
args:
  - name: --uncommitted
    description: Mode flag. Review working-tree changes vs HEAD. The four mode flags (`--uncommitted`, `--base`, `--commit`, `--pr`) are mutually exclusive; exactly one is required for any non-help invocation (loop or side-effect). `--help` / `-h` is the sole exception — it requires no mode flag and short-circuits before any other validation. Passing two or more mode flags raises `UsageError`.
  - name: --base BRANCH
    description: Mode flag. Review the current branch vs BRANCH.
  - name: --commit SHA
    description: Mode flag. Review one commit by 40-hex SHA.
  - name: --pr NUM
    description: Mode flag. Review a PR's changes by number.
  - name: --level LVL
    description: Configured floor for the run. One of low|medium|high|xhigh. Default low. Persisted in the manifest under the field name `start_level` (this document uses both names interchangeably). Part of the resume key `(target, start_level)`: on later invocations the recorder resumes only when this value matches; otherwise it starts a new run (no error).
  - name: --ceiling LVL
    description: Top of the reasoning ladder. All-clean at this level halts `DoneFixedPoint` directly without a Retrospective handoff. Default xhigh, set by the binary's CLI parser. Must be >= --level (UsageError otherwise). Levels are totally ordered low < medium < high < xhigh.
  - name: -n N
    description: Parallel review count. Default 3, must be ≥1. Recorded in the manifest as `batch_size`. Not part of the resume key.
  - name: --max-iter N
    description: Loop-iteration cap. Default 50, must be ≥1. Ignored by side-effect invocations.
  - name: --state-root PATH
    description: Directory for batch logs and recorder state. Default $TMPDIR/ooda-codex-review.
  - name: --codex-bin PATH
    description: Path to the `codex` binary. Default `codex` (PATH lookup).
  - name: --criteria STRING
    description: "Free-form review prompt forwarded to `codex review` as a positional argument inserted after codex's own target flag (which this binary derives from its own mode flag — `--uncommitted` becomes `codex review --uncommitted`, `--base BRANCH` becomes `codex review --base BRANCH`, etc.). Default omitted; codex uses its built-in criteria."
  - name: --fresh
    description: Ignore the `latest` pointer; force a new run. Loop-mode only. Combining with any side-effect flag is a UsageError (the side-effect would mutate a brand-new manifest with no review history; semantics are not defined).
  - name: --mark-retro-clean
    description: |
      Side-effect. Orchestrator reports the retrospective at the
      current level produced no architectural changes. Always
      records the per-level outcome (variant `Clean`) before
      dispatching. Dispatch is by **strict equality** of
      `current_level` against the configured ceiling:

      | `current_level`                                 | Behavior                                          | Outcome                   |
      | :---------------------------------------------- | :------------------------------------------------ | :------------------------ |
      | strictly below configured ceiling               | advance one rung toward the ceiling               | `Idle` (exit 7)           |
      | == configured ceiling                           | no level change; halt terminal-success            | `DoneFixedPoint` (exit 0) |
      | strictly above configured ceiling AND < xhigh   | advance one rung (further above the ceiling)      | `Idle` (exit 7)           |
      | == xhigh AND `current != configured ceiling`    | no level change (ladder edge no-op)               | `Idle` (exit 7)           |

      Rows 3 and 4 are only reachable when out-of-band
      `--advance-level` pushed `current_level` above the configured
      ceiling. To recover, call `--restart-from-floor` (which
      resets current_level to the configured floor); a single
      `--mark-retro-clean` will not emit `DoneFixedPoint` from any
      above-ceiling state.
  - name: --mark-retro-changes REASON
    description: "Side-effect. Orchestrator reports the retrospective surfaced architectural changes (the orchestrator should implement those changes BEFORE invoking this flag — the flag closes the level out and resets the recorder, it does not gate the implementation). Records the per-level outcome (variant `RetrospectiveChanges`) with REASON, then restarts from the configured floor. Outcome: `Idle` (exit 7)."
  - name: --mark-address-passed
    description: "Side-effect. Orchestrator reports the address agent fixed the batch and the project's tests passed. Records the per-level outcome (variant `Addressed`) — issue count derived from the current batch's `.log` files via the same verdict-classify pass observe uses; 0 if no batch is present. Then drops one rung, clamped at the configured floor — at the floor this is a no-op transition but the outcome still records. Outcome: `Idle` (exit 7) regardless of clamp. Rationale (per the loop-codex-review meta-procedure this binary implements): drop one level after a passing fix so the next iteration re-runs reviews at lower reasoning to confirm the patch passes simpler scrutiny too."
  - name: --mark-address-failed DETAILS
    description: "Side-effect. Orchestrator reports post-address tests failed. Does NOT record any per-level outcome and does NOT transition the level — purely an escalation. Outcome: `HandoffHuman` (exit 3). The stderr action kind on the header line is `TestsFailedTriage`. The prompt is a fixed template that interpolates the manifest's current_level (the level the addressed batch ran at) and DETAILS verbatim: `Tests failed after addressing review batch at level <LEVEL>. Surface to a human for triage. Details: <DETAILS>`. Unlike the recording marks, this side-effect writes nothing to stdout."
  - name: --advance-level
    description: "Side-effect (low-level primitive). Bump manifest current_level by one rung along the full reasoning ladder (low<medium<high<xhigh). Bounded by the **ladder edge xhigh**, NOT the configured ceiling — so when configured ceiling < xhigh it can push current_level above the configured ceiling. To return to a clean state from out-of-band overshoot the orchestrator should call `--restart-from-floor` (a single `--mark-retro-clean` only advances one more rung at most and never emits DoneFixedPoint while above the configured ceiling). Outcome `Idle` (exit 7) on success and on ladder-top no-op. Prefer `--mark-retro-clean`; this primitive is provided for tests and direct manipulation and never emits `DoneFixedPoint` itself."
  - name: --drop-level
    description: "Side-effect (low-level primitive). Drop one rung, clamped at the configured floor (`--level`). Outcome `Idle` (exit 7) on success and on floor no-op. Prefer `--mark-address-passed`."
  - name: --restart-from-floor
    description: "Side-effect (low-level primitive). Reset current_level to the configured floor. Outcome `Idle` (exit 7). Prefer `--mark-retro-changes`."
  - name: -h, --help
    description: Print usage to stdout and exit 0. Standalone short-circuit; not an invocation mode and does not require a mode flag. Distinguishable from `DoneFixedPoint` (also exit 0) by checking for `--help`/`-h` in the argv the orchestrator itself issued, OR by stderr being empty (loop-mode `DoneFixedPoint` writes a `DoneFixedPoint` header to stderr).
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
  act, up to `--max-iter` iterations, until the binary returns one
  of: `DoneFixedPoint`, `StuckRepeated`, `StuckCapReached`,
  `HandoffAgent`, `BinaryError`, or `Idle` (decide produced no
  candidate action). Loop mode never returns `HandoffHuman` today
  (only `--mark-address-failed` produces it); `WouldAdvance` and
  `DoneAborted` are reserved exit codes the binary cannot
  currently produce. A single binary invocation runs as many
  iterations as needed within `--max-iter`; after the binary
  returns, the orchestrator's outer loop re-invokes the binary —
  for `HandoffAgent` after the dispatched agent + follow-up
  `--mark-*` complete; for `Idle` immediately to drive the next
  observe; for `DoneFixedPoint`, `StuckRepeated`, `StuckCapReached`,
  and `BinaryError`, the orchestrator stops the loop and surfaces
  the result.

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
  with `--fresh` (UsageError if combined). All non-help
  invocations require a mode flag for context (the recorder resume
  key is `(target, start_level)`, where `start_level` is the
  manifest field for the configured floor — see Resume semantics).

The **configured floor** (`--level`) is part of the resume key, so
side-effect invocations must pass the same `--level` that the loop
invocation used; otherwise the recorder will treat them as a
different ladder and start a fresh run with no ladder history. (If
`--level` is omitted, the resolved floor is `low` — the default —
which must also match.) (See the `--level` arg description for the persistence detail —
the configured floor is the manifest field `start_level`. Both
names are used interchangeably across this document.)

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

| Step                              | Procedural (this binary)            | LLM (orchestrator)                                                                                                                 |
| --------------------------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Discover repo root                | `git rev-parse --show-toplevel`     |                                                                                                                                    |
| Spawn n codex reviews             | `RunReviews` (Full)                 |                                                                                                                                    |
| Poll log files for marker         | `AwaitReviews` (Wait; 30s default)  |                                                                                                                                    |
| Extract verdict + classify        | `scan_batch` heuristic              |                                                                                                                                    |
| Verify + address batch, run tests |                                     | `HandoffAgent: AddressBatch` → orchestrator runs the agent + tests, reports via `--mark-address-passed` or `--mark-address-failed` |
| Synthesize retrospective          |                                     | `HandoffAgent: Retrospective` → orchestrator runs the agent, reports via `--mark-retro-clean` or `--mark-retro-changes`            |
| Climb / drop / restart            | recorder mutation inside `--mark-*` |                                                                                                                                    |

The poll cadence is 30s by default; the `OODA_AWAIT_SECS` env var
overrides it (intended for tests; orchestrators should leave it
unset in production).

## Outcomes (exit codes)

The boundary contract is **one variant → one exit code**. The
orchestrator dispatches on `$?` for the coarse routing decision; for
`HandoffAgent` (exit 5) and `HandoffHuman` (exit 3) it also reads
the action name from the first line of stderr to pick a branch (see
"Streams contract" below). All other exit codes can be dispatched
from `$?` alone.

| Code | Variant           | Producer(s)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ---- | ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0    | `DoneFixedPoint`  | Loop mode: a per-level batch completes with all reviews clean while the current level equals the configured ceiling. Side-effect mode: `--mark-retro-clean` while the current level equals the configured ceiling.                                                                                                                                                                                                                                                                                    |
| 1    | `StuckRepeated`   | Loop mode: the same `(action kind, blocker key)` pair fired on two consecutive non-Wait iterations. Wait actions (e.g. `AwaitReviews` polling) are excluded from this check; cycling on Wait is normal. The stderr header carries the action kind and blocker key of the repeated action.                                                                                                                                                                                                             |
| 2    | `StuckCapReached` | Loop mode: `--max-iter` iterations elapsed without a halt. The stderr header carries the action kind and blocker key of the **last attempted action** (which may be a Wait, since the cap can fire on a polling iteration).                                                                                                                                                                                                                                                                           |
| 3    | `HandoffHuman`    | Side-effect: `--mark-address-failed`. (Loop mode never selects a `HandoffHuman` action today; this is the only producer.)                                                                                                                                                                                                                                                                                                                                                                             |
| 4    | `WouldAdvance`    | **Reserved**, not producible. Future inspect-mode (single-pass observe + decide, no act) would map a non-halt decision into this code. There is currently no CLI surface to invoke it.                                                                                                                                                                                                                                                                                                                |
| 5    | `HandoffAgent`    | Loop mode: a per-level batch completes with at least one review flagging issues → `AddressBatch` (regardless of current level vs. ceiling); a per-level batch completes with all reviews clean while `current_level != configured ceiling` → `Retrospective` (whether current is below OR strictly above ceiling — above-ceiling is reachable via out-of-band `--advance-level`). The stderr header carries the action name (`AddressBatch` or `Retrospective`); the orchestrator reads it to branch. |
| 6    | `BinaryError`     | Caught external failure (codex spawn failed, IO error, recorder open failed). The header line is `BinaryError: <message>`.                                                                                                                                                                                                                                                                                                                                                                            |
| 7    | `Idle`            | Loop mode: decide had nothing to do this iteration. Side-effect mode: any of `--advance-level`, `--drop-level`, `--restart-from-floor`, `--mark-retro-clean` (when current is at any level **other than** the configured ceiling — both below AND above), `--mark-retro-changes`, `--mark-address-passed` completing successfully.                                                                                                                                                                    |
| 8    | `DoneAborted`     | **Reserved**, not producible. The variant exists for future SIGINT / abort handling; no trigger is wired today.                                                                                                                                                                                                                                                                                                                                                                                       |
| 64   | `UsageError`      | CLI parse / validation failure (unknown flag, invalid value, conflicting flags, `--fresh` combined with a side-effect, etc.).                                                                                                                                                                                                                                                                                                                                                                         |

## Streams contract

| Stream | Always carries                                                  | Sometimes carries                                                                                                                                                                                              |
| ------ | --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| stdout | nothing for loop mode                                           | help text (for `--help`); a one-line resolution message before the Outcome header (for side-effect invocations only — see "Side-effect resolution lines" below)                                                |
| stderr | the Outcome header (one line) for every Outcome except `--help` | an indented `  prompt: <description>` line for `HandoffAgent` / `HandoffHuman`; the full multi-line usage block for `UsageError` (printed to stderr after the header so orchestrators can show it to the user) |
| `$?`   | the exit code (the machine contract)                            |                                                                                                                                                                                                                |

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

| Placeholder     | Meaning                                                                                                                                                                                 |
| :-------------- | :-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `<ActionKind>`  | The decide-layer action name. Stderr-header value; today emitted as `RunReviews`, `AwaitReviews`, `AddressBatch`, `Retrospective`, or `TestsFailedTriage` (others reserved/internal).   |
| `<LEVEL>`       | A reasoning-level token (`low` / `medium` / `high` / `xhigh`). Substituted with the manifest's `current_level` at the moment of invocation unless the per-template note says otherwise. |
| `<NEXT_LEVEL>`  | The reasoning-level token after the transition. Direction depends on the template: advance → one rung up; drop → one rung down. The placeholder name is direction-agnostic.             |
| `<FLOOR>`       | The configured floor (`--level`'s value, or `low` if `--level` was omitted).                                                                                                            |
| `<CEILING>`     | The configured ceiling (`--ceiling`'s value, or `xhigh` if `--ceiling` was omitted).                                                                                                    |
| `<COUNT>`       | An integer issue count (number of reviewers in the current batch whose verdict classified as has-issues; `0` if no batch is present).                                                   |
| `<REASON>`      | Free-form orchestrator-supplied REASON string passed to `--mark-retro-changes`.                                                                                                         |
| `<DETAILS>`     | Free-form orchestrator-supplied DETAILS string passed to `--mark-address-failed`.                                                                                                       |
| `<message>`     | Caught-error message embedded in `BinaryError: <message>` (codex spawn / IO / recorder open failure detail).                                                                            |
| `<blocker key>` | Stable iteration-key string the binary uses to detect StuckRepeated. Free-form per-action; orchestrators should treat the suffix after `<ActionKind>:` as opaque.                       |

### Side-effect resolution lines (format templates)

Six of the seven side-effect flags emit a single advisory line to
stdout before the Outcome header lands on stderr.
`--mark-address-failed` is the exception: it emits nothing on
stdout (only the `HandoffHuman: TestsFailedTriage` header on
stderr).

The format templates below use the placeholders defined above.
Runtime values are substituted directly (no quote escaping; if
the orchestrator-supplied REASON or DETAILS contains a literal
`"`, it appears verbatim in the resolution line and may break
naive parsers).

| Trigger flag            | Template                                                                                                                                                                                                                                                                                              |
| :---------------------- | :---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--advance-level`       | `advanced level: <LEVEL> -> <NEXT_LEVEL>`                                                                                                                                                                                                                                                             |
| `--advance-level`       | `at ceiling (xhigh); no advance` _(when current is already at xhigh — the ladder edge. `--advance-level` walks the full reasoning ladder, so "ceiling" in this template means xhigh, not the user's `--ceiling`. The level token is always literal `xhigh` in this branch.)_                          |
| `--drop-level`          | `dropped level: <LEVEL> -> <NEXT_LEVEL>`                                                                                                                                                                                                                                                              |
| `--drop-level`          | `at floor (<LEVEL>); no drop` _(when current is already at floor)_                                                                                                                                                                                                                                    |
| `--restart-from-floor`  | `restarted from floor: <LEVEL> -> <FLOOR>`                                                                                                                                                                                                                                                            |
| `--mark-retro-clean`    | `retrospective clean at ceiling (<CEILING>); fixed point reached` _(at configured ceiling — paired with DoneFixedPoint exit)_                                                                                                                                                                         |
| `--mark-retro-clean`    | `retrospective clean at <LEVEL>; advanced to <NEXT_LEVEL>` _(when `current != configured ceiling` and `current < xhigh` — covers both the normal below-ceiling case AND the out-of-band above-ceiling-but-below-xhigh case where `--advance-level` pushed past `--ceiling`. Outcome: `Idle` exit 7.)_ |
| `--mark-retro-clean`    | `retrospective clean at <LEVEL>; ladder ceiling reached, no advance` _(when current is at xhigh and `current != configured ceiling` — fires only when `--advance-level` pushed current up to xhigh past the configured ceiling. "ladder ceiling" here means xhigh. Outcome: `Idle` exit 7.)_          |
| `--mark-retro-changes`  | `retrospective surfaced changes at <LEVEL> ("<REASON>"); restarted from floor <FLOOR>`                                                                                                                                                                                                                |
| `--mark-address-passed` | `address passed at <LEVEL> (<COUNT> issue(s)); dropped to <NEXT_LEVEL>`                                                                                                                                                                                                                               |
| `--mark-address-passed` | `address passed at floor <LEVEL> (<COUNT> issue(s)); no drop` _(at configured floor)_                                                                                                                                                                                                                 |

These resolution lines are advisory diagnostics; orchestrators
should dispatch on `$?` plus the stderr header, not parse stdout.

### Stderr example (HandoffAgent, the most common multi-line case)

```
HandoffAgent: AddressBatch
  prompt: Verify and address 2 review(s) with issues at level low...
```

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
                <L>-2.log
                ...
                <L>-<N>.log
      latest                     pointer file → <run-id>
```

- `<L>` is `low | medium | high | xhigh` — the per-level subdirectory
  name and the per-log filename prefix both use the same token.
- `<N>` is the configured `-n` (parallel review count); each batch
  contains exactly N log files, one per spawned reviewer.
- `<repo-id>` = `<repo-basename>-<sha256(remote-url)[..12]>` (or
  `<repo-basename>-noremote` when no remote is configured).
- `<target-key>` = `uncommitted` | `base/<branch>` | `commit/<sha>` |
  `pr/<num>`. Branch names containing `/` (e.g. `feature/x`) are
  written verbatim — the recorder relies on the filesystem to
  handle nested path segments.
- `<run-id>` = `<utc-second>-<nanos>-p<pid>` where `<utc-second>`
  is `YYYYMMDDTHHMMSSZ` (RFC 3339-derived, second precision),
  `<nanos>` is a 9-digit zero-padded nanosecond field of the same
  instant, and `<pid>` is the process id. Sortable
  lexicographically; the nanosecond field is what makes parallel
  invocations from the same host collision-resistant (pid is a
  human-readable suffix, not the uniqueness guarantee).
- `latest` is a plain text file containing only the active run-id,
  written without a trailing newline. Tools should `trim()` before
  comparing.

## Orchestration recipe

This pseudocode is the canonical loop. It captures every exit code
the binary can currently produce, including the codes returned by
`--mark-*` follow-up calls. Every `break` exits the outer
`loop forever`.

```
MODE = "--uncommitted"            # or --base <BRANCH> / --commit <SHA> / --pr <NUM>
LEVEL = "--level low"             # the configured-floor flag+value; pass the same
                                  # value to every invocation so side-effect calls
                                  # hit the same resume key
CEILING = "--ceiling xhigh"       # propagate to side-effect calls so --mark-retro-clean
                                  # uses the same DoneFixedPoint check the loop used.
                                  # If you accept the default, omit (default is xhigh).
ARGS = MODE + " " + LEVEL + " " + CEILING

loop forever:
  exit, stderr := run("ooda-codex-review " + ARGS)

  switch exit:

    0:  # DoneFixedPoint — ceiling-level all-clean reached in loop mode
      break

    5:  # HandoffAgent — read stderr action kind to branch
        # stderr first line is exactly "HandoffAgent: <ActionKind>"
        # (ActionKind is one of AddressBatch, Retrospective).
      kind := stderr_first_line.removeprefix("HandoffAgent: ")
      prompt := stderr_indented_block_after("  prompt: ")

      switch kind:

        "AddressBatch":
          dispatch_address_agent(prompt)
          if run_project_tests() == PASS:
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-address-passed")
            # m_exit is always 7 (Idle); next iteration drives the next observe
            assert m_exit == 7
          else:
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-address-failed " + shell_quote(test_summary))
            # m_exit is 3 (HandoffHuman); surface m_stderr and stop the loop
            assert m_exit == 3
            surface_to_human("TestsFailedTriage", stderr_indented_block_after("  prompt: "), m_stderr)
            break

        "Retrospective":
          patterns := dispatch_retro_agent(prompt)
          if patterns.is_empty:
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-retro-clean")
            # at ceiling -> m_exit == 0 (DoneFixedPoint), break outer loop
            # below ceiling -> m_exit == 7 (Idle), continue
            if m_exit == 0: break
          else:
            implement(patterns)
            m_exit, m_stderr := run("ooda-codex-review " + ARGS +
                                    " --mark-retro-changes " + shell_quote(reason))
            # m_exit is always 7 (Idle); next iteration starts at floor
            assert m_exit == 7

    3:  # HandoffHuman. Read the action kind from the stderr header
        # (format: "HandoffHuman: <ActionKind>") and the prompt from
        # the indented "  prompt: " line. Today the only emitted kind
        # is TestsFailedTriage (from --mark-address-failed). The
        # canonical recipe handles --mark-address-failed inline in
        # case 5, so this top-level handler is defensive against
        # orchestrators that call --mark-address-failed at the top of
        # the loop and against future paths (inspect mode, a wired
        # --criteria refinement halt).
      kind := stderr_first_line.removeprefix("HandoffHuman: ")
      prompt := stderr_indented_block_after("  prompt: ")
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
on exit code and reports outcomes back via `--mark-*`. The four
`--mark-*` flags split into two groups:

- **Recording marks** (`--mark-retro-clean`, `--mark-retro-changes`,
  `--mark-address-passed`) atomically record a per-level outcome
  AND apply the appropriate ladder change (advance, drop, restart,
  or — for `--mark-retro-clean` at ceiling — terminal halt with no
  ladder change). All three exit `Idle` except `--mark-retro-clean`
  at ceiling, which exits `DoneFixedPoint`.
- **Escalation mark** (`--mark-address-failed`) records nothing and
  does not transition; it only emits `HandoffHuman` so the
  orchestrator can surface the failure with a structured prompt.

The low-level primitives `--advance-level`, `--drop-level`, and
`--restart-from-floor` are provided for tests and direct
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

| Reason             | Trigger                                                                                                                                                  |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Forced             | `--fresh` was passed (loop mode only; `--fresh` + side-effect is a UsageError, not a resume).                                                            |
| NoLatestPointer    | First invocation for this `(repo-id, target-key)` — no `latest` exists yet.                                                                              |
| LatestDangling     | `latest` points to a `<run-id>` directory that no longer exists.                                                                                         |
| ManifestUnreadable | `manifest.json` is missing or fails to deserialize.                                                                                                      |
| LevelMismatch      | The manifest's recorded `start_level` differs from the invocation's resolved `--level` (which is the explicit value, or `low` if `--level` was omitted). |

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
- It does not synthesize the retrospective. The orchestrator
  dispatches the agent and reports back via
  `--mark-retro-clean` or `--mark-retro-changes`.

It DOES own the ladder transitions: each `--mark-*` invocation
records the outcome and applies the right transition (advance,
drop, restart-from-floor) atomically.

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
ceiling-level Retrospective handoff. The orchestrator can still
record a ceiling-level Retrospective out-of-band via
`--mark-retro-clean` (which yields `DoneFixedPoint` in turn), if
its own workflow produced one.

A "full fixed point" therefore means: every level from floor to
ceiling has reached all-clean, and (for sub-ceiling levels) the
orchestrator's retrospective produced no architectural changes (i.e.
each was reported via `--mark-retro-clean`, not
`--mark-retro-changes`).

## Examples

```bash
# Default loop: review uncommitted changes, floor=low, ceiling=xhigh,
# 3 reviewers per level
ooda-codex-review --uncommitted

# Loop: current branch vs master, start at medium, climb only to high
ooda-codex-review --base master --level medium --ceiling high -n 5

# Loop: focus the review with a custom criteria string
ooda-codex-review --uncommitted --criteria "check for SQL injection"

# Loop: a specific PR, max 20 iterations
ooda-codex-review --pr 1234 --max-iter 20

# Force a brand-new run, ignoring the latest pointer
ooda-codex-review --uncommitted --fresh

# Side-effect: orchestrator reports tests passed after AddressBatch
ooda-codex-review --uncommitted --mark-address-passed

# Side-effect: orchestrator reports tests failed (will exit
# HandoffHuman / 3 with DETAILS embedded in the prompt)
ooda-codex-review --uncommitted --mark-address-failed "test_X failed at line 42: expected ok, got Err(...)"

# Side-effect: orchestrator reports retrospective clean
# (Idle below ceiling; DoneFixedPoint at ceiling)
ooda-codex-review --uncommitted --mark-retro-clean

# Side-effect: orchestrator implemented retrospective patterns
ooda-codex-review --uncommitted --mark-retro-changes "Found N+1 pattern"
```
