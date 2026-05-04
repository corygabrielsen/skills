---
name: ooda-codex-review
description: Drive `codex review` to fixed point across the reasoning ladder. Each invocation observes the run dir, decides one action, optionally spawns reviews, and emits exactly one Outcome the caller dispatches on. 1:1 variant→exit-code; dispatch on `$?` alone.
args:
  - name: --uncommitted
    description: Mode flag. Review working-tree changes vs HEAD. Mutually exclusive with --base/--commit/--pr.
  - name: --base BRANCH
    description: Mode flag. Review the current branch vs BRANCH. Mutually exclusive with the other mode flags.
  - name: --commit SHA
    description: Mode flag. Review one commit by 40-hex SHA.
  - name: --pr NUM
    description: Mode flag. Review a PR's changes by number.
  - name: --level LVL
    description: Reasoning level (= floor for the run). One of low|medium|high|xhigh. Default low. Recorded as `start_level` in the manifest; resume requires this match.
  - name: --ceiling LVL
    description: Top of the reasoning ladder. All-clean here halts `DoneFixedPoint`. Default xhigh. Must be >= --level.
  - name: -n N
    description: Parallel review count. Default 3, must be ≥1. Recorded in the manifest.
  - name: --max-iter N
    description: Loop iteration cap. Default 50, must be ≥1.
  - name: --state-root PATH
    description: Directory for batch logs and recorder state. Default $TMPDIR/ooda-codex-review.
  - name: --codex-bin PATH
    description: Path to the `codex` binary. Default `codex` (PATH lookup).
  - name: --criteria STRING
    description: Free-form review prompt passed to `codex review` as a positional argument after the mode flag (e.g. `codex review --uncommitted "check for SQL injection" -c ...`). Default omitted; codex uses its built-in criteria.
  - name: --fresh
    description: Ignore the `latest` pointer; start a new run. Otherwise the recorder resumes the prior run when target+start_level match.
  - name: --mark-retro-clean
    description: "Side-effect. Orchestrator reports retrospective produced no changes. Records `LevelOutcome::Clean`. At ceiling -> DoneFixedPoint; below ceiling -> advance + Idle. Mutually exclusive with other side-effects."
  - name: --mark-retro-changes REASON
    description: Side-effect. Orchestrator reports retrospective surfaced architectural changes. Records `LevelOutcome::RetrospectiveChanges` and restarts from floor. Idle.
  - name: --mark-address-passed
    description: Side-effect. Orchestrator reports the address agent fixed the batch and tests passed. Records `LevelOutcome::Addressed` (issue count from current batch verdicts) and drops one rung (clamp at floor). Idle.
  - name: --mark-address-failed DETAILS
    description: Side-effect. Orchestrator reports post-address tests failed. No transition; emits `HandoffHuman` with DETAILS as the prompt.
  - name: --advance-level
    description: Side-effect (low-level). Bump manifest current_level by one. Idle at ceiling. Prefer `--mark-retro-clean`.
  - name: --drop-level
    description: Side-effect (low-level). Drop one rung, clamp at floor. Idle at floor. Prefer `--mark-address-passed`.
  - name: --restart-from-floor
    description: Side-effect (low-level). Reset current_level to floor. Idle. Prefer `--mark-retro-changes`.
  - name: -h, --help
    description: Print usage to stdout and exit 0. The only invocation that writes to stdout (side-effect transitions also write a one-line resolution to stdout).
---

# /ooda-codex-review

Drives `codex review` to fixed point across the reasoning ladder
(low → medium → high → xhigh). Each invocation runs one OODA pass —
observe the run dir, decide one action, optionally spawn — and emits
exactly one `Outcome` the orchestrator dispatches on by exit code.

The reasoning content lives in the orchestrator (the outer Claude
session), not in this binary. The binary is a stateless step
function: it spawns `codex review` subprocesses, polls their log
files, and halts with structured handoffs when an LLM is needed
(verify-and-address, retrospective synthesis).

## Architecture: where the smarts live

| Step                       | Procedural (this binary) | LLM (orchestrator)                         |
| -------------------------- | ------------------------ | ------------------------------------------ |
| Detect base branch         | `git rev-parse`          |                                            |
| Spawn n codex reviews      | `RunReviews` Full action |                                            |
| Poll log files             | `AwaitReviews` Wait{30s} |                                            |
| Extract verdict + classify | `scan_batch` heuristic   |                                            |
| Verify + address batch     |                          | `HandoffAgent(AddressBatch)`               |
| Run tests                  |                          | (orchestrator drives, then `--drop-level`) |
| Retrospective              |                          | `HandoffAgent(Retrospective)`              |
| Climb / drop / restart     | recorder mutation flags  |                                            |

## Outcomes (exit codes)

The boundary contract is **one variant → one exit code**. The
orchestrator dispatches on `$?` alone — no string parsing.

| Code | Variant           | Meaning                                                                                                                   |
| ---- | ----------------- | ------------------------------------------------------------------------------------------------------------------------- |
| 0    | `DoneFixedPoint`  | All n clean at the ceiling level; retrospective produced no changes. Terminal success.                                    |
| 1    | `StuckRepeated`   | Same `(kind, blocker)` action fired twice consecutively. Stall detector tripped.                                          |
| 2    | `StuckCapReached` | `--max-iter` reached without a halt.                                                                                      |
| 3    | `HandoffHuman`    | Decide selected an action requiring human input (e.g. test failure triage).                                               |
| 4    | `WouldAdvance`    | Inspect-mode only (not yet wired). Decide selected an Execute action.                                                     |
| 5    | `HandoffAgent`    | Decide selected an action requiring an agent (`AddressBatch` or `Retrospective`). The action's description is the prompt. |
| 6    | `BinaryError`     | Caught external failure (codex spawn failed, IO error).                                                                   |
| 7    | `Idle`            | No-op — decide had nothing to do, or a `--advance-level` / `--drop-level` / `--restart-from-floor` mutation completed.    |
| 8    | `DoneAborted`     | User aborted the loop.                                                                                                    |
| 64   | `UsageError`      | CLI parse / validation failure.                                                                                           |

Stderr format (single-line header per Outcome):

```
HandoffAgent: AddressBatch
  prompt: Verify and address 2 review(s) with issues at level low...
```

`Handoff*` variants append a `prompt: ...` block; everything else is
a single line.

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
                low-1.log
                low-2.log
                ...
      latest                     pointer file → <run-id>
```

- `<repo-id>` = `<repo-basename>-<sha256(remote-url)[..12]>` (or
  `<repo-basename>-noremote`).
- `<target-key>` = `uncommitted` | `base/<branch>` | `commit/<sha>` |
  `pr/<num>`.
- `<run-id>` = `<utc>-<nanos>-pid` — sortable, parallel-safe.
- `latest` is a plain text file containing the active run-id.

## Orchestration recipe

```
loop:
  exit, stderr := run("ooda-codex-review --uncommitted")
  case exit:
    0 (DoneFixedPoint): break    # done
    5 (HandoffAgent):
      action := parse stderr     # AddressBatch | Retrospective
      run agent on action.prompt
      if action == AddressBatch:
        run tests
        if tests pass:
          run("ooda-codex-review --uncommitted --mark-address-passed")
        else:
          run("ooda-codex-review --uncommitted --mark-address-failed '<details>'")
          break
      else:  # Retrospective
        if retro found patterns:
          implement
          run("ooda-codex-review --uncommitted --mark-retro-changes '<reason>'")
        else:
          run("ooda-codex-review --uncommitted --mark-retro-clean")
    3 (HandoffHuman): surface to human; break
    1 | 2 (Stuck*):   surface; break
    6 (BinaryError):  surface; break
```

The binary owns the state transitions; the orchestrator only
dispatches on exit code and reports outcomes back via `--mark-*`.
Each `--mark-*` invocation atomically:

1. Records a `LevelOutcome` in the manifest's `level_history`
2. Applies the appropriate ladder transition (advance, drop,
   restart-from-floor, or none)
3. Returns the documented Outcome

The lower-level `--advance-level` / `--drop-level` /
`--restart-from-floor` flags remain for testing and direct
manipulation, but the `--mark-*` flags are the orchestrator API.

## Resume semantics

By default each invocation resumes the prior run named by
`<target_root>/latest`. The recorder accepts the resume only when
the manifest's `(target, start_level)` matches the invocation's;
otherwise it creates a new run and returns the diagnostic in
`OpenMode::Fresh(FreshReason)`:

| Reason               | Trigger                          |
| -------------------- | -------------------------------- |
| `Forced`             | `--fresh` was set                |
| `NoLatestPointer`    | first invocation for this target |
| `LatestDangling`     | pointer named a missing run dir  |
| `ManifestUnreadable` | manifest missing or corrupt      |
| `LevelMismatch`      | manifest's start_level differed  |

`--fresh` forces a new run unconditionally.

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

The default ladder is `low → medium → high → xhigh`. Per
loop-codex-review semantics, the floor (`--level`) and the
ceiling (defaulted to `xhigh` in the orchestrator's logic)
bound where the loop walks. Drops are clamped at the floor;
restarts reset to it.

A full fixed point requires all n reviews clean at the ceiling AND
the retrospective at the ceiling producing no architectural
changes. Each per-level fixed point triggers a Retrospective
handoff so the orchestrator can synthesize patterns across levels.

## Examples

```bash
# Default: review uncommitted changes, low level, 3 reviewers,
# climb to xhigh
ooda-codex-review --uncommitted

# Review the current branch vs master, start at medium, climb
# only to high
ooda-codex-review --base master --level medium --ceiling high -n 5

# Review a specific PR, max 20 iterations
ooda-codex-review --pr 1234 --max-iter 20

# Orchestrator: after AddressBatch handoff completes and tests pass
ooda-codex-review --uncommitted --mark-address-passed

# Orchestrator: tests failed after addressing
ooda-codex-review --uncommitted --mark-address-failed "test_X failed: ..."

# Orchestrator: retrospective found no architectural changes
ooda-codex-review --uncommitted --mark-retro-clean

# Orchestrator: retrospective found patterns; orchestrator
# implemented them and now restarts the loop
ooda-codex-review --uncommitted --mark-retro-changes "Found N+1 pattern"

# Force a brand-new run, ignoring resume
ooda-codex-review --uncommitted --fresh
```
