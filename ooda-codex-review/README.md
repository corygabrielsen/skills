# `/ooda-codex-review` — Type Algebra

Single-binary OODA loop driving `codex review` to fixed point across
the reasoning ladder. Anti-DRY sibling of [`ooda-pr`](../ooda-pr/)
retargeted at codex review subprocesses instead of GitHub PR
convergence; `observe/codex/{batch,verdict}.rs` are byte-identical
with [`ooda-pr-codex-review`](../ooda-pr-codex-review/) (codex-pair
mirror tier, this crate canonical).

This document is the **type-level specification**. For invocation
and exit-code taxonomy see [`SKILL.md`](./SKILL.md). For
implementation see `src/`.

## Top Level

```
ids ⊕ observe ⊕ orient ⊕ decide ⊕ act ⊕ runner ⊕ signal ⊕ outcome
        ⊕ ooda-core (type spine) ⊕ ooda-state (run tree)

main      : Argv → Outcome → ExitCode
            ExitCode = Outcome.exit_code()    (1:1 variant → code)

run_loop  : RepoId × ReviewTarget × LoopConfig × ActContext
              × Observe × EventSink
          → Result⟨LoopExit, LoopError⟩

LoopExit  = Halted(HaltReason) | SignalInterrupted { exit_code: u8 }
LoopError = Observe(String) | Act(ActError)
LoopConfig = { max_iterations: NonZeroU32, ceiling: CodexReasoningLevel }
```

The binary is a **stateless step function**. Each invocation: open a
fresh run → observe filesystem → orient → decide → optionally act →
emit one Outcome. Cross-iteration ladder position is orchestrator
state, passed in via `--level` on every invocation.

Two invocation modes share `main`:

```
side_effect = None     → run_loop                   (loop mode)
side_effect = Some(_)  → apply_side_effect          (one event, one Outcome)
```

## Shared boundary types (`ooda-core` crate)

This binary depends on the sibling [`ooda-core`](../ooda-core/)
library crate for the cross-binary type spine: `Outcome`,
`Decision`, `DecisionHalt`, `HaltReason`, `Terminal`, `Action`,
`HandoffAction`, `ActionEffect`, `UpstreamConsistency`, `Urgency`,
`MidTier`, `TargetEffect`, `BlockerKey`, `GateIdentity`,
`StallKey`, `HandoffPrompt`, `PollingInterval`,
`SingleLineString`, `ExitCode`, and the `ActionKindName` trait.
Each generic-over-`ActionKind` type is instantiated locally via a
type alias:

```rust
pub type Outcome      = ooda_core::Outcome<ActionKind>;
pub type Decision     = ooda_core::Decision<ActionKind>;
pub type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub type HaltReason   = ooda_core::HaltReason<ActionKind>;
pub type Action       = ooda_core::Action<ActionKind>;
```

The codex-review-domain `ActionKind` enum, its
`ActionKindName::name()` impl, and `CodexReasoningLevel` (the
ladder type) live in `decide/action.rs`.

**Variant name ≠ stderr header.** Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are neutral verbs
shared with the sibling OODA binaries. Stderr headers emitted by
this binary's `render_outcome` are the codex-review vocabulary
callers see; the same projection is applied to the `run_halted`
event's `outcome` token by `ooda_state::CodexReviewDomain`:

| Variant         | Stderr header    | Exit |
| --------------- | ---------------- | :--: |
| `DoneSucceeded` | `DoneFixedPoint` |  0   |
| `Paused`        | `Idle`           |  1   |
| `DoneAborted`   | `DoneAborted`    |  5   |

The exit code is the formal contract; the stderr text is
codex-flavoured documentation. The variant column in the
Outcome section below uses ooda-core names.

## Domain primitives (`ids`)

Every identifier is a validated newtype.

| Type                  | Shape                                                                                                                            |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `RepoId`              | `<basename>-<sha256(key)[..12]>`, `key = <remote-url>@<toplevel>` or `noremote@<toplevel>`; ASCII printable, no `/`              |
| `ReviewTarget`        | `Uncommitted` \| `Base(BranchName)` \| `Commit(GitCommitSha)` \| `Pr(u64)`                                                       |
| `BranchName`          | git ref-name validated                                                                                                           |
| `GitCommitSha`        | 40 lowercase hex                                                                                                                 |
| `Timestamp`           | RFC-3339, ordered on the instant                                                                                                 |
| `CodexReasoningLevel` | `Low < Medium < High < Xhigh`; `higher()` / `lower()` partial on the edges; `GateIdentity`                                       |
| `BlockerKey`          | re-export of `ooda_core::BlockerKey`; `from_static(&'static str)` or `typed(category, &impl GateIdentity)` → `"<category>:<id>"` |

`Pr(u64)` is caller-facing identity, not a direct codex target.
Loop mode resolves it with `gh pr view NUM --json baseRefName` and
spawns codex with `--base <baseRefName>`; `build_codex_args`
rejects an unresolved `Pr` with `ActError::UnsupportedTarget`.

## O = observe

```
observe::codex::fetch_all
  : RepoId × ReviewTarget × Path × CodexReasoningLevel × u32
  → io::Result⟨CodexObservations⟩

CodexObservations =
  { repo_id, target, current_level, batch_state, batch_dir, expected }

scan_batch : Path × CodexReasoningLevel × expected × Option⟨identity⟩
           → io::Result⟨BatchState⟩          (this crate passes None)

BatchState =
    NotStarted
  | Running { pending_slots: Vec<PendingSlot>, completed_verdicts: Vec<VerdictRecord> }
  | Complete { verdicts: Vec<VerdictRecord> }
  | InconsistentState { total, completed, expected, reason }

PendingSlot   = { slot: u32, log_mtime: SystemTime, log_bytes: u64 }
VerdictRecord = { slot: u32, body: String, class: VerdictClass }
VerdictClass  = Clean | HasIssues | Indeterminate | Abandoned

ALIVE_THRESHOLD = 90s
```

Reads scoped to `batch_dir` under a cooperative advisory lock
(`FileLock` on `.batch.lock`; the same lock the spawn path holds
while truncating logs). No subprocess, no network.

Slot reduction, per `<level>-<slot>.log` with optional sibling
`<level>-<slot>.exit`:

```
no .exit                             → pending (mtime, size recorded)
.exit ≠ 0                            → io::Error
.exit = 0 ∧ no `^codex$` marker      → io::Error
.exit = 0 ∧ marker ∧ empty body      → io::Error
.exit = 0 ∧ marker ∧ non-empty body  → VerdictRecord
.exit without matching .log          → io::Error

completed < expected → Running
completed = expected → Complete
completed > expected → InconsistentState
no files for level   → NotStarted
```

Verdict body = suffix after the LAST line exactly equal to `codex`.
`classify` evaluates in order: empty → `Clean`; line-anchored
structural marker (`- [p1]`…`[p3]`, `review comment:`,
`full review comments:`) → `HasIssues`; whitelisted clean phrasing
→ `Clean`; otherwise `Indeterminate`. `Abandoned` is never produced
by `classify`; only by `BatchState::project_abandoning_pending`,
which maps `Running` to a synthetic `Complete` with one
`Abandoned` verdict per pending slot.

## O = orient

```
orient : CodexObservations × CodexReasoningLevel → OrientedState

OrientedState = { current_level, ceiling, batch_state, expected }
```

Forwarding layer. `ceiling` comes from `LoopConfig` (set by the
`--ceiling` flag, default `xhigh`, must be ≥ `--level`); decide
consults it to recognize ceiling-level fixed points.

## D = decide

```
decide : OrientedState → Decision        (via ooda_core::classify)

Decision =
    Execute(Action)
  | Halt(DecisionHalt)

DecisionHalt =
    Success
  | Terminal(Terminal)         -- Succeeded | Aborted  (Succeeded is the codex fixed point)
  | AgentNeeded(HandoffAction)
  | HumanNeeded(HandoffAction)

Action        = { kind: ActionKind, effect: ActionEffect, target_effect, urgency, blocker }
HandoffAction = { kind: ActionKind, prompt: HandoffPrompt, target_effect, urgency, blocker }

ActionEffect =
    Full  { log, upstream: UpstreamConsistency }
  | Wait  { interval: PollingInterval, log }
  | Agent { prompt: HandoffPrompt }
  | Human { prompt: HandoffPrompt }

UpstreamConsistency = Sync | Eventual(PollingInterval)
TargetEffect        = Blocks | Advances | Neutral
Urgency             = Pre ⊕ Mid(MidTier) ⊕ Post
MidTier             = Critical < Pathology < BlockingFix < BlockingWait
                      < BlockingHuman < Advancing < Hygiene
```

`ActionKind` (12 variants; `name()` is the payload-free token that
appears in stderr headers and `events.jsonl`):

```
ActionKind                                 effect               target    urgency         blocker
  RunReviews { level, n }                  Full, Eventual(60s)  Advances  Mid(Critical)   runreviews:<level>
  AwaitReviews { level, pending }          Wait, interval       Neutral   BlockingWait    await:<level>
  AddressBatch { issue_count, level }      Agent                Blocks    BlockingFix     address:<level>
  Retrospective { level }                  Agent                Advances  BlockingFix     retro:<level>
  BatchStateInconsistent { level, reason } Human                Blocks    Pathology       inconsistent:<level>
  TestsFailedTriage                        Human                Blocks    BlockingHuman   address-failed
  ParseVerdicts { level }                  declared; no constructor (parsing is implicit in observe)
  AdvanceLevel { from, to }                declared; no constructor
  DropLevel { from, to }                   declared; no constructor
  RestartFromFloor { reason }              declared; no constructor
  RunTests                                 declared; no constructor
  RequestCriteriaRefinement                declared; no constructor
```

`TestsFailedTriage` is constructed as a `HandoffAction` by
`--mark-address-failed` in `main`, not by decide. The `AwaitReviews`
interval is `OODA_AWAIT_SECS` (positive integer) or 30s.

Decide first applies the alive/idle discriminator, then the
transition table:

```
Running ∧ ∃ pending slot with now − log_mtime ≤ ALIVE_THRESHOLD  → keep Running
Running ∧ ∀ pending slots idle past ALIVE_THRESHOLD              → project_abandoning_pending

match (batch_state, current_level == ceiling):
  (NotStarted,                    _)     → Execute(RunReviews { level, n: expected })
  (Running,                       _)     → Execute(AwaitReviews { level, pending: expected − completed })
  (InconsistentState { reason },  _)     → Halt(HumanNeeded(BatchStateInconsistent { level, reason }))
  (Complete { ∀ Clean },          true)  → Halt(Terminal(Succeeded))          -- codex fixed point
  (Complete { ∀ Clean },          false) → Halt(AgentNeeded(Retrospective { level }))
  (Complete { otherwise },        _)     → Halt(AgentNeeded(AddressBatch { issue_count, level }))
      issue_count = #{ v | v.class ∈ { HasIssues, Indeterminate, Abandoned } }
```

`Terminal(Aborted)` and `Success` have no producer in this crate's
decide.

Cross-iteration ladder transitions are NOT emitted by decide. They
are computed by the `--advance-level` / `--drop-level` /
`--restart-from-floor` / `--mark-*` side-effect flags against the
`--level` the orchestrator passes in (see `state` below).

Blocker keys are level-scoped so two `RunReviews` at different
levels are distinct iterations; the payload (`n`, `pending`,
`issue_count`) is excluded from the key by construction.

## A = act

```
act : Action × ActContext → Result⟨(), ActError⟩

ActContext = { batch_dir, target, repo_root, codex_bin,
               spawned_pgids: Arc<Mutex<Vec<i32>>> }

ActError = UnsupportedAutomation | UnsupportedTarget(String) | NotImplemented
         | Spawn { slot: u32, source: io::Error }
```

Dispatch on `effect`:

- `Wait { interval }` → sleep `interval`.
- `Full` ∧ `RunReviews { level, n }` → `spawn_codex_reviews`.
- `Full` ∧ any other kind → `NotImplemented`.
- `Agent` / `Human` → `UnsupportedAutomation` (invariant violation;
  decide halts these before act).

`spawn_codex_reviews`:

```
secure_create_dir_all(batch_dir); FileLock(batch_dir/.batch.lock)
preflight: codex_bin must exist when absolute or multi-component
argv = build_codex_args(level, target)
     = ["review", <target-args>, "-c", "model_reasoning_effort=\"<level>\""]
       target-args: --uncommitted | --base <b> | --commit <sha> | Pr → UnsupportedTarget
∀ slot ∈ 1..=n:
  truncate <level>-<slot>.log (0600); unlink <level>-<slot>.exit
  spawn /bin/sh -c 'umask 077; "$@" > $OODA_LOG_PATH 2>&1; code=$?;
                    printf "%s\n" "$code" > $OODA_EXIT_PATH; exit "$code"'
        in a fresh process group (pgid = wrapper pid), cwd = repo_root
  push pgid onto spawned_pgids; detached thread waits the child
return immediately
```

Completion is observed, not awaited: the `.exit` file appears only
after the child terminates. Partial-spawn failure leaves
already-spawned children running; the next observe sees `Running`.

`reap_spawned_children(ctx, grace)`: `killpg(SIGTERM)` every
recorded pgid, sleep `grace`, `killpg(SIGKILL)`; clears the
registry; idempotent. Bound to a `ChildReaper` RAII guard on the
`run_loop` frame with `grace = 2s`, so every exit path (halt,
signal, observe error, panic) reaps once.

`build_codex_args` is pure so the argv shape is unit-testable
without spawning.

## runner

```
run_loop : ... → Result⟨LoopExit, LoopError⟩

HaltReason =
    Decision(DecisionHalt)
  | Stalled(Action)        -- same StallKey on consecutive non-Wait iterations
  | CapReached(Action)     -- --max-iter hit

StallKey = { kind_name: &'static str, blocker: BlockerKey }   -- payload-free
```

Per iteration: `signal::check_shutdown()` at the boundary →
observe → orient → `IterationOriented` → decide →
`IterationDecided` → (Halt ⇒ return) | (Execute ⇒ stall check →
act → `IterationExecuted` / `IterationWaited` → error bubbles).

Stall comparator on a non-Wait repeat of `last_non_wait_key`:

```
auto_wait_used_for == key                       → Halt(Stalled)
effect.synthetic_wait_on_repeat() = Some(wait)  → substitute Wait{interval, log}, proceed
   (Full with upstream = Eventual(interval))
otherwise (Full with Sync)                      → Halt(Stalled)
```

A Wait whose key equals `last_non_wait_key` is by construction the
synthetic conversion and arms `auto_wait_used_for = key`; a
non-Wait with a different key resets it. `RunReviews` is the only
`Full{Eventual}` kind, so one 60s synthetic Wait is granted before
its second consecutive repeat halts. Natural Waits (`AwaitReviews`)
never participate in the comparator.

Iteration 1 is structurally guaranteed (`NonZeroU32`), so
`last_attempted` is a typed `Action`. At the cap:

```
last_attempted = AwaitReviews ∧ observe ok ∧ project_abandoning_pending = Some(c)
    → decide({ batch_state: c, ..oriented })
        Halt(h)    → Halted(Decision(h))
        Execute(a) → Halted(CapReached(a))
otherwise → Halted(CapReached(last_attempted))
```

`EventSink` writes `IterationOriented { blob }` (the
`OrientedState` snapshot), `IterationDecided { decision_kind }`
(`Execute` ⇒ `kind.name()`; halts ⇒ `ooda_state::DecisionKind`
tokens `Halt::Success`, `Halt::Terminal(Succeeded)`,
`Halt::Terminal(Aborted)`, `Halt::AgentNeeded`,
`Halt::HumanNeeded`), `IterationWaited { action_kind, interval_ms }`
for Wait effects, and `IterationExecuted { action_kind, success }`
otherwise, `success` recorded before an act error bubbles. Sink
write failures are swallowed; the exit code is the authoritative
signal.

## signal

```
install_signal_handlers : () → io::Result⟨()⟩     -- first statement of main
SHUTDOWN_SIGNAL : AtomicI32                        -- 0 | 130 (SIGINT) | 143 (SIGTERM)
check_shutdown  : () → Option⟨u8⟩                  -- polled at iteration boundaries
```

Handlers do one atomic store. The loop owns the halt path, so the
terminal event and live-marker release land on the same write path
as every other halt.

## state (`ooda-state` crate)

There is no per-binary recorder module. `main::run_session` drives
an `ooda_state::RunWriter` directly; `runner::EventSink` wraps it
for per-iteration events.

```
run_session : Args → Outcome

state_root = --state-root | $OODA_STATE_HOME | $XDG_STATE_HOME/ooda
           | $HOME/.local/state/ooda | $TMPDIR/ooda
StateRoot::new → sweep_dead_markers → RunId::generate → create_run
  → RunStarted { domain: "codex-review",
                 target: { mode, value, floor, ceiling } }
       mode ∈ uncommitted | base | commit | pr | side-effect

loop mode:
  repo_root   = git rev-parse --show-toplevel        (10s, 4 KiB caps)
  repo_id     = compute_repo_id(repo_root)
  codex_target = Pr(n) ↦ Base(gh pr view n …)         (60s, 256 KiB caps)
  batch_dir   = <state-root>/runs/<run-id>/scratch
  observe     = fetch_all(repo_id, target, batch_dir, --level, --codex-review-n)
  Outcome     = run_loop(…) ↦ Halted(h) ⇒ From⟨HaltReason⟩
                             | SignalInterrupted { exit_code }
                             | Err(e) ⇒ From⟨LoopError⟩
side-effect mode: no git, no gh; apply_side_effect(--level, --ceiling)

finalize : RunWriter × Outcome → ()
  DomainSpecific { kind_suffix: "outcome", payload: { exit_code, blob } }
  then terminal_event:
    StuckRepeated   → RunStalled    { last_action: kind.name() }
    StuckCapReached → RunCapReached { last_action: kind.name() }
    otherwise       → RunHalted     { outcome: <header token>, exit_code }
```

Layout (one tree per invocation; nothing shared across runs, no
resume protocol):

```
<state-root>/
  runs/<run-id>/
    events.jsonl                  append-only typed events
    blobs/<sha>.<ext>             orient snapshots (json), outcome (json), handoff prompts (md)
    scratch/
      <L>-<slot>.log              spawned by act, scanned by observe
      <L>-<slot>.exit
      .batch.lock.lock            FileLock sidecar
  live/<run-id>                   presence = active
```

Side-effect flags each emit one event against the `--level` rung
(`L`) and return the documented Outcome. `--level` is both the floor
and the current rung; `higher()` is bounded by the ladder edge
`xhigh`, not by `--ceiling`.

| Flag                               | `decision_kind`                                                                    | stdout line                                                                               | Outcome                           |
| ---------------------------------- | ---------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | --------------------------------- |
| `--advance-level`                  | `AdvanceLevel`                                                                     | `advanced level: L -> L+1` \| `at ladder edge (L); no advance`                            | `Paused`                          |
| `--drop-level`                     | `DropLevel`                                                                        | `dropped level: L -> L-1` \| `at floor (L); no drop`                                      | `Paused`                          |
| `--restart-from-floor`             | `RestartFromFloor`                                                                 | `restarted to floor: L`                                                                   | `Paused`                          |
| `--mark-retro-clean` (L = ceiling) | `RetroClean::Terminal`                                                             | `retrospective clean at ceiling (L); fixed point reached`                                 | `DoneSucceeded`                   |
| `--mark-retro-clean` (L ≠ ceiling) | `RetroClean::Advance`                                                              | `retrospective clean at L; advanced to L+1` \| `…; ladder edge xhigh reached, no advance` | `Paused`                          |
| `--mark-retro-changes REASON`      | `RetroChanges::RestartFromFloor`                                                   | `retrospective surfaced changes ("REASON"); restarted to floor: L`                        | `Paused`                          |
| `--mark-address-passed`            | `AddressPassed`                                                                    | `address passed at L; dropped to L-1` \| `address passed at floor L; no drop`             | `Paused`                          |
| `--mark-address-failed DETAILS`    | `IterationHandoff { variant: HandoffHuman, action_kind: TestsFailedTriage, blob }` | (none)                                                                                    | `HandoffHuman(TestsFailedTriage)` |

Any `RunWriter` append failure in a side-effect path returns
`BinaryError` instead of the documented Outcome.

## CLI (`main`)

```
Args = { target: Option<ReviewTarget>, level, ceiling, n: NonZeroU32,
         max_iter: NonZeroU32, state_root: Option<PathBuf>, codex_bin,
         side_effect: Option<SideEffect> }

SideEffect = AdvanceLevel | DropLevel | RestartFromFloor | MarkRetroClean
           | MarkRetroChanges(String) | MarkAddressPassed | MarkAddressFailed(String)

UsageError ⇐ >1 target flag | >1 side-effect flag | --criteria present
           | no target ∧ no side-effect | --ceiling < --level
           | clap parse failure | --state-root not an existing directory
--help / -h → usage on stdout, exit 0, no Outcome
```

## outcome

```
Outcome =                              exit  stderr
    DoneSucceeded                        0   "DoneFixedPoint"
  | Paused                               1   "Idle"
  | WouldAdvance(Box<Action>)            2   "WouldAdvance: <kind>"          -- From⟨Decision⟩ not wired to any CLI path
  | HandoffHuman(Box<HandoffAction>)     3   "HandoffHuman: <kind>" ⏎ "  prompt: <prompt>"
  | HandoffAgent(Box<HandoffAction>)     4   "HandoffAgent: <kind>" ⏎ "  prompt: <prompt>"
  | DoneAborted                          5   "DoneAborted"                   -- no producer in this crate
  | StuckRepeated(Box<Action>)           6   "StuckRepeated: <kind>:<blocker>"
  | StuckCapReached(Box<Action>)         7   "StuckCapReached: <kind>:<blocker>"
  | UsageError(SingleLineString)        64   "UsageError: <msg>" ⏎ usage block
  | BinaryError(SingleLineString)       70   "BinaryError: <msg>"
  | SignalInterrupted { exit_code }  130/143 "Interrupted: exit code <n>"

From⟨HaltReason⟩  : loop-mode collapse        [blanket impl in ooda-core]
                    Success ↦ Paused; Terminal(Succeeded) ↦ DoneSucceeded;
                    Terminal(Aborted) ↦ DoneAborted; AgentNeeded ↦ HandoffAgent;
                    HumanNeeded ↦ HandoffHuman; Stalled ↦ StuckRepeated;
                    CapReached ↦ StuckCapReached
From⟨Decision⟩    : inspect-mode collapse     [blanket impl in ooda-core] (not wired)
From⟨LoopError⟩   : Outcome::binary_error     [per-binary impl; SingleLineString flattens newlines]
```

`HandoffPrompt` renders as markdown: `# <headline>` followed by
sections; this crate's prompts are headline-only. The exit-code
mapping is the binary's contract. Callers dispatch on `$?` alone;
`HandoffAgent` / `HandoffHuman` callers additionally read `<kind>`
from the header to pick a branch.

## Conventions

- **State containment**: every file this binary writes lives under
  `<state-root>/runs/<run-id>/`; no process-wide mutable state
  beyond the signal atomic.
- **Read/write separation**: observe reads `scratch/` under the
  batch lock; subprocess spawn lives only in act; git / gh probes
  live only in `main` before the loop starts.
- **Branch responsibility**: `--pr` resolves a PR's base branch for
  the reviewer; the caller owns the checkout state of the worktree
  under review.
- **Typed boundaries**: identifiers are newtypes; no raw `String`s
  cross module boundaries. `BlockerKey` is gate-stable by
  construction (`from_static` / `typed` over a `GateIdentity`).
- **Child lifetime ≤ parent lifetime**: every spawned codex process
  group is reaped when `run_loop` returns, on any path.
