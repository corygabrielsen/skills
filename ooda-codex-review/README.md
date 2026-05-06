# `/ooda-codex-review` — Type Algebra

Single-binary OODA loop driving `codex review` to fixed point across
the reasoning ladder. Anti-DRY copy of [`ooda-pr`](../ooda-pr/)
retargeted at codex review subprocesses instead of GitHub PR
convergence. They will eventually unify once a third sibling exists
(rule of three).

This document is the **type-level specification**. For invocation
and exit-code taxonomy see [`SKILL.md`](./SKILL.md). For
implementation see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ recorder ⊕ outcome

main      : Argv → Outcome → ExitCode
            ExitCode = Outcome.exit_code()    (1:1 variant → code)

run_loop  : RepoId × ReviewTarget × LoopConfig × ActContext
              × Observe × OnState
          → Result⟨HaltReason, LoopError⟩

Recorder  : RecorderConfig → (Recorder × OpenMode)
            advance_level / drop_level / restart_from_floor / record_outcome
```

The binary is a **stateless step function**. Each invocation: open
recorder → observe filesystem → orient → decide → optionally act →
emit one Outcome. Cross-iteration logic lives in the outer Claude
session that dispatches on the exit code.

## Domain primitives (`ids`)

Every identifier is a validated newtype.

| Type           | Shape                                                                      |
| -------------- | -------------------------------------------------------------------------- |
| `RepoId`       | `<basename>-<sha256(remote)[..12]>` or `<basename>-noremote`               |
| `ReviewMode`   | `Uncommitted` \| `Base` \| `Commit` \| `Pr`                                |
| `ReviewTarget` | `Uncommitted` \| `Base(BranchName)` \| `Commit(GitCommitSha)` \| `Pr(u64)` |
| `BranchName`   | git ref-name validated                                                     |
| `GitCommitSha` | 40 lowercase hex                                                           |
| `BlockerKey`   | non-empty string; stable iteration key for stall detection                 |

`Pr(u64)` is user-facing recorder identity, not a direct codex
target. Loop mode resolves the PR base branch with `gh pr view` and
spawns codex with `--base <baseRefName>`.

## O = observe

```
observe::codex::fetch_all
  : RepoId × ReviewTarget × Path × ReasoningLevel × u32
  → io::Result⟨CodexObservations⟩

CodexObservations =
  { repo_id, target, current_level, batch_state, batch_dir, expected }

BatchState =
    NotStarted
  | Running { total: u32, completed: u32 }
  | Complete { verdicts: Vec<VerdictRecord> }

VerdictRecord = { slot: u32, body: String, class: VerdictClass }
VerdictClass  = Clean | HasIssues
```

Pure read of the run dir. `verdict::extract_verdict` is the
awk-equivalent (last `^codex$` marker wins; non-empty body required
for completion). `batch::scan_batch` walks `<batch_dir>/{level}-*.log`
plus matching `.exit` files and produces the `BatchState`.
Nonzero child exits and zero exits without a non-empty verdict block
return an IO error so the runner emits `BinaryError` instead of
waiting forever.

## O = orient

```
orient : CodexObservations × ReasoningLevel → OrientedState

OrientedState = { current_level, ceiling, batch_state, expected }
```

Forwarding layer. `ceiling` comes from `LoopConfig` (set by the
`--ceiling` flag, defaults to `xhigh`); decide consults it to
recognize ceiling-level fixed points. Cross-iteration recorder
state (level_history, test outcomes) is materialized in the
manifest and consulted directly by the `--mark-*` side-effect
commands rather than by decide.

## D = decide

```
decide : OrientedState → Decision

Decision =
    Execute(Action)
  | Halt(DecisionHalt)

DecisionHalt =
    Success
  | Terminal(Terminal)         -- FixedPoint | Aborted
  | AgentNeeded(Action)
  | HumanNeeded(Action)

Action = { kind: ActionKind, automation, target_effect, urgency,
           description, blocker }

ActionKind =
    RunReviews { level, n }            Full,  Critical
  | AwaitReviews { level, pending }    Wait,  BlockingWait
  | ParseVerdicts { level }            Full,  Critical          (unused; implicit in observe)
  | AddressBatch { issue_count, level} Agent, BlockingFix
  | Retrospective { level }            Agent, BlockingFix
  | AdvanceLevel { from, to }          Full,  Critical          (reserved; not emitted)
  | DropLevel { from, to }             Full,  Critical          (reserved; not emitted)
  | RestartFromFloor { reason }        Full,  Critical          (reserved; not emitted)
  | RunTests                           Full,  Critical          (reserved; not emitted)
  | RequestCriteriaRefinement          Human, BlockingHuman
```

In-batch state machine:

```
match (batch_state, current_level == ceiling):
  (NotStarted,                _)     → Execute(RunReviews)
  (Running { c < expected },  _)     → Execute(AwaitReviews)
  (Complete { all clean },    true)  → Halt(Terminal(FixedPoint))
  (Complete { all clean },    false) → Halt(AgentNeeded(Retrospective))
  (Complete { has issues },   _)     → Halt(AgentNeeded(AddressBatch))
```

Cross-iteration ladder transitions are NOT emitted by decide.
They live in the orchestrator-facing `--mark-*` CLI surface, which
calls `Recorder::{advance_level, drop_level, restart_from_floor}`
directly. The reserved `AdvanceLevel`/`DropLevel`/etc.
`ActionKind` variants exist for a future full-OODA mode where
decide would emit them based on recorder state surfaced in
`OrientedState`.

Blocker keys are level-scoped so two RunReviews at different levels
are distinct iterations. AwaitReviews is exempt from stall detection
(Wait automation) so polling cycles freely.

## A = act

```
act : Action × ActContext → Result⟨(), ActError⟩

ActContext = { batch_dir, target, repo_root, codex_bin }

ActError = UnsupportedAutomation | UnsupportedTarget | NotImplemented
         | Spawn { slot: u32, source: io::Error }
```

`Wait` sleeps the configured interval. `Full` dispatches by
ActionKind:

- `RunReviews` → synchronously create `<batch_dir>/<level>-<slot>.log`,
  spawn `n` wrapper subprocesses, redirect child stdout/stderr to
  the log, write `<level>-<slot>.exit` on child completion, then
  return immediately
- (other Full kinds: NotImplemented; the ladder transitions
  happen via `--mark-*` invocations that call the recorder
  directly, not via decide-emitted actions)

`Agent`/`Human` are an invariant violation in act — decide should
have halted instead.

`build_codex_command` is split out as a pure function so the argv
shape is unit-tested without spawning.

## runner

```
run_loop : ... → Result⟨HaltReason, LoopError⟩

HaltReason =
    Decision(DecisionHalt)
  | Stalled(Action)        -- same (kind, blocker) twice in a row
  | CapReached(Action)     -- --max-iter hit
```

Stall detection tracks the last non-Wait action per `(kind,
blocker)`. The cap is the second line of defense.

## recorder

```
RunManifest =
  { run_id, repo_id, mode, target_key, start_level, current_level,
    batch_size, batch_number, level_history, created_at }

LevelOutcome =
    Clean { level }
  | Addressed { level, issue_count }
  | RetrospectiveChanges { level, reason }

OpenMode = Fresh(FreshReason) | Resumed
FreshReason = Forced | NoLatestPointer | LatestDangling
            | ManifestUnreadable | LevelMismatch
```

Layout (one tree per `(repo, target)`):

```
<state-root>/<repo-id>/<target-key>/
  latest                           pointer
  runs/<run-id>/
    manifest.json
    levels/level-<L>/batch-<n>/
      <L>-<slot>.log
      <L>-<slot>.exit
```

Resume rules — all must hold:

- `cfg.fresh` is false
- `latest` exists, non-empty
- the run dir it names exists
- `manifest.json` parses
- `manifest.start_level == cfg.start_level`

Mutations (`advance_level`, `drop_level`, `restart_from_floor`,
`start_next_batch_at_current_level`, `record_outcome`) write the
manifest immediately. Level transitions select the next unused
`batch_number` for the destination level; same-level rebatching
selects the next unused batch at the current level. `record_outcome`
appends to `level_history` without touching `current_level`.

The orchestrator-facing CLI exposes these through `--mark-*`
subcommands. Each one combines a `record_outcome` call with the
appropriate level transition and returns a documented Outcome:

| Flag                                 | Records                               | Transition                          | Outcome             |
| ------------------------------------ | ------------------------------------- | ----------------------------------- | ------------------- |
| `--mark-retro-clean` (at ceiling)    | `Clean(level)`                        | none                                | `DoneFixedPoint`    |
| `--mark-retro-clean` (below ceiling) | `Clean(level)`                        | `advance_level`                     | `Idle`              |
| `--mark-retro-changes REASON`        | `RetrospectiveChanges(level, reason)` | `restart_from_floor`                | `Idle`              |
| `--mark-address-passed`              | `Addressed(level, issue_count)`       | `drop_level` or next batch at floor | `Idle`              |
| `--mark-address-failed DETAILS`      | (none)                                | none                                | `HandoffHuman(...)` |

## outcome

```
Outcome =
    DoneFixedPoint           0
  | StuckRepeated(Action)    1
  | StuckCapReached(Action)  2
  | HandoffHuman(Action)     3
  | WouldAdvance(Action)     4    -- inspect mode (not wired)
  | HandoffAgent(Action)     5
  | BinaryError(String)      6
  | Idle                     7
  | DoneAborted              8
  | UsageError(String)       64

From⟨HaltReason⟩  : loop-mode collapse
From⟨Decision⟩    : inspect-mode collapse (not wired)
From⟨LoopError⟩   : caught external failure
```

The exit-code mapping is the binary's contract. Callers dispatch on
`$?` alone — no string parsing.

## Conventions

- All filesystem operations scoped to `state_root`. No global state.
- Subprocess spawn lives in `act`, not `observe`. Observe is
  read-only filesystem.
- `--pr` is resolved to a base branch; branch checkout remains the
  caller's responsibility.
- Identifiers are newtypes — no raw `String`s cross module
  boundaries.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings`
  enforced via pre-commit.
