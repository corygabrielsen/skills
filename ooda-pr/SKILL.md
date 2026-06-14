---
name: ooda-pr
description: Drive a PR through observe → orient → decide → act. Each invocation produces exactly one Outcome the caller dispatches on. 1:1 variant-to-exit-code; dispatch on `$?` alone.
args:
  - name: <owner/repo>
    description: GitHub repository slug. Required positional.
  - name: <pr>
    description: PR number. Required positional.
  - name: inspect
    description: Optional subcommand. If used, must precede the two positionals; flags may come before it. Runs one observe/orient/decide pass; no act, no loop.
  - name: --max-iter N
    description: Loop iteration cap. Default 50; must be ≥1 (validation runs in all modes before mode dispatch; --max-iter 0 is rejected as UsageError even in inspect mode). Inspect mode runs exactly one pass and does not consult the cap value.
  - name: --status-comment
    description: Post a status comment to the PR each iteration. Deduped per-PR via the always-on state root.
  - name: --state-root PATH
    description: Override the always-on local state root for this invocation.
  - name: --repo-root PATH
    description: Target working tree for every `gt`/`git` subprocess. Default derived from CWD via `git rev-parse --show-toplevel`; invocations from outside any git tree are rejected as UsageError unless this flag is supplied.
  - name: --trace PATH
    description: Also append the compact trace to PATH. Always-on state is written even when this is omitted.
  - name: -h, --help
    description: Print usage to stdout and exit 0. The only invocation that writes to stdout.
---

# /ooda-pr

Drives one PR through observe → orient → decide → act. Each
invocation returns one `Outcome`; the caller dispatches on the
exit code alone.

## Names

| Name       | Refers to                                                                                                                                                                                                                    |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/ooda-pr` | The skill (this document, invoked from a Claude Code agent prompt).                                                                                                                                                          |
| `ooda-pr`  | The compiled Rust binary. The `run` wrapper resolves the symlink via `pwd -P` and locates the binary at `target/release/ooda-pr` inside the resolved source directory. Callers should invoke `run`, not the binary directly. |
| `run`      | The wrapper script at `~/.claude/skills/ooda-pr/run`.                                                                                                                                                                        |

Always invoke `run`; never the binary directly. `run` performs
the rebuild step (`cargo build --release --quiet`) before
exec'ing the binary, so the binary path is fresh whenever `run`
last completed. Invoking the binary directly skips the rebuild
and may serve a stale build relative to the current source
tree.

## Sibling binaries

| Name          | Refers to                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ooda-attest` | Companion CLI that writes attestation files at `<state-root>/<pr-id>/<axis>_attest.json` for four axes: `pr-meta`, `doc-review`, `claude-review`, `closeout`. Each `Sync*` / `Review*` / `Address*` / `Closeout` handoff (exit 4) instructs the receiving agent to run the matching subcommand after completing the work. `closeout` is the convergence gate — it fires only when every other axis is silent and gates `HandoffHuman` on an explicit agent sign-off. Run `ooda-attest --help` for the full surface. |

## Type spine

Boundary types are defined in the `ooda-core` library crate
(`/home/cory/code/skills/ooda-core/`) and shared with the three
sibling OODA binaries. This binary depends on `ooda-core` via
path dep and instantiates each generic type over its
domain-specific `ActionKind` enum:

```rust
pub type Outcome      = ooda_core::Outcome<ActionKind>;
pub type Decision     = ooda_core::Decision<ActionKind>;
pub type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub type HaltReason   = ooda_core::HaltReason<ActionKind>;
pub type Action       = ooda_core::Action<ActionKind>;
```

`Automation`, `Urgency`, `TargetEffect`, `BlockerKey`, `Terminal`,
and the `ActionKindName` trait are re-exported from `ooda-core`
directly. `ActionKind` is per-binary — this binary's variants
cover the PR-merge domain (`FixCi`, `AddressThreads`, `Rebase`,
…) and implement `ActionKindName` so the loop can render variant
tokens for iter-log lines and stderr headers without leaking
payload internals.

**Variant name ≠ stderr header.** The Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are internal — the
neutral verbs that fit every binary in the family. The stderr
header strings (`DoneMerged`, `DoneClosed`, `Paused`) are this
binary's caller contract and are emitted by the per-binary
`render_outcome` function. The Outcomes table below shows both
columns; callers dispatch on `$?` and read the stderr header,
not the variant name.

**Per-binary code (not lifted):**

- `runner.rs::run_loop` — iteration sequencing, stall detection,
  cap detection. Each binary's runner diverges enough on
  side-effects / flock / refresh logic that lifting is premature.
- `state.rs` — thin adapter over the shared `ooda-state` crate.
  PR-specific event vocabulary (`action_started`,
  `status_comment_rendered`, `tool_call_finished`, …) lives here;
  the generic on-disk layout (events.jsonl + content-addressed
  blobs) is owned by `ooda-state`.
- `decide/action.rs::ActionKind` and its `ActionKindName` impl.
- `From<LoopError> for Outcome` — `LoopError` shape differs per
  binary.

See `ooda-core/README.md` and `ooda-core/src/lib.rs` for the
shared-spine design rationale.

## Calling discipline

**`$?` MUST reflect ooda-pr's exit when ooda-pr runs.** Two
distinct concerns:

1. **ooda-pr must actually run.** `false && ooda-pr ...`
   short-circuits and ooda-pr never executes; `$?` will reflect
   the left side's exit, not ooda-pr's. Structure invocations so
   ooda-pr runs unconditionally.
2. **Nothing may inject another exit code into `$?`.** Pipes
   (`ooda-pr | foo`), stderr-merging pipes (`ooda-pr |&`,
   `ooda-pr 2>&1 | foo`), backgrounding (`ooda-pr &`), command
   substitution (`out=$(ooda-pr ...)`), and any subsequent
   command (`ooda-pr; echo x`) replace `$?` with another
   process's exit.

**Safe patterns:**

- ooda-pr alone on the line: `~/.claude/skills/ooda-pr/run owner/repo 42`
- ooda-pr as the last command after `;`: `pwd; ~/.claude/skills/ooda-pr/run owner/repo 42`
- ooda-pr on the right of `&&` after a reliably-succeeding left side: `cd /tmp && ~/.claude/skills/ooda-pr/run owner/repo 42`

**Capturing stderr:** redirect to a file (`ooda-pr ... 2>file`)
for a single invocation. For repeated invocations, use append
redirection (`2>>file`), a fresh file per run, or `--trace PATH`.
File redirection does not affect `$?`. Process substitution
(`ooda-pr 2> >(tee file)`) also preserves `$?`. The forms that
break `$?` are stderr **piping** (`|&`, `2>&1 |`). `--trace PATH`
is now an extra compatibility sink; the durable audit trail is
always written under the state root.

**Separate Bash tool calls in the same agent turn are fine** —
each Bash call is an independent shell with its own `$?`.

## Driving discipline

Loop mode is meant to run to a halt. The only correct stopping
points are the halt-class exit codes: `0` (`DoneMerged`), `1`
(`Paused`), `3` (`HandoffHuman`), `4` (`HandoffAgent`), `5`
(`DoneClosed`), `6` (`StuckRepeated`), `7` (`StuckCapReached`),
and `70` (`BinaryError`). Stopping anywhere else — including
after exit `2` (`WouldAdvance`) — is premature.

**Anti-patterns that stop the loop early.** These are common
agent mistakes; the binary is not at fault.

- **Probing repeatedly with `inspect`.** Inspect is a one-shot
  snapshot. After the first inspect (or as the very first call
  on an unfamiliar PR), drop `inspect` and re-invoke as the
  loop. Re-`inspect`-ing in place of running the loop is the
  most common way agents stall a PR.
- **Treating `WouldAdvance` as a halt.** Exit 2 is **inspect-only**.
  The action shown is what the loop _would_ do; re-invoke without
  `inspect` to actually do it. Do not report `WouldAdvance` to the
  user and stop.
- **Re-`inspect`-ing after a `Handoff*` action completes.** The
  action's effect needs to land in fresh observation, which the
  _next loop iteration_ will do — not the next inspect. After a
  Handoff returns and you complete the requested action, re-invoke
  in **loop mode** (no `inspect`).
- **Shrinking `--max-iter` as a "safety" cap.** The default (50)
  exists because `Wait` iterations (15s/30s/60s) are how the loop
  polls slow external systems (CI runs, bot reviews, scheduled
  jobs). Capping at 3, 5, or 10 routinely converts a normal wait
  into a spurious `StuckCapReached` (exit 7). Use the default
  unless you have a specific reason; if exit 7 fires from a
  wait-heavy run, re-invoke with a higher cap, not a lower one.
- **Inferring "stuck" from long wait runs.** A run that spends
  minutes in `Wait(1m)` polling for a CI check or a bot review is
  working correctly. The wait is the action. Let it finish.
- **Surfacing to the user mid-loop after a `Handoff*`.** When you
  complete a Handoff action, the next tool call MUST be either
  another `~/.claude/skills/ooda-pr/run …` invocation (re-enter
  the loop) or an explicit user-directed action that genuinely
  requires user input. Do NOT summarize the situation, offer
  options, or ask whether to continue. The user invoked
  `/ooda-pr` _because_ they don't want to make those
  decisions per-round. Editorializing about "diminishing
  returns" or "marginal value" is an anti-pattern — those
  judgments are encoded in the halt-class exit codes, not in
  agent vibes.
- **Treating your own context budget as a halt signal.** The
  harness handles compaction; trust it. The agent should never
  stop the loop because _it_ is worried about its own context
  — that's confusing the agent's resource constraints with
  the user's intent. If context is genuinely tight, finish the
  in-flight Handoff action, re-invoke, and let the natural
  halt code (or compaction itself) be the pause point.
- **Reading "each round still finds something" as
  divergence.** Convergence under loop mode is
  bugs-fixed-per-round, not findings-per-round. A loop that
  finds and addresses 6 → 5 → 4 → 3 → 2 → 1 → halt is
  converging exactly as intended. Don't bail because the bot
  reviewer keeps surfacing new threads — that's the loop
  _working_.

**After a `Handoff*` (exit 3 or 4).** Surface the handoff to the
user (header + iter-log + handoff blob; see `Handoff*` prompt
format → "Surface to the user"). Then perform the requested
action and re-invoke `/ooda-pr` in **loop mode** (no `inspect`).
The loop's first iteration re-observes the now-modified state and
either selects the next action or halts.

**Time budget.** ooda-pr is iteration-bounded, not wall-clock-bounded.
A loop run can legitimately take 30+ minutes if external systems
are slow. Plan for that; don't artificially cut it short. If you
genuinely need a wall-clock deadline, impose it externally — but
expect that doing so will produce spurious `StuckCapReached`
results, not faster convergence.

## How to call

```bash
~/.claude/skills/ooda-pr/run [options] <owner/repo> <pr>           # loop mode
~/.claude/skills/ooda-pr/run inspect [options] <owner/repo> <pr>   # one pass
```

**Argument rules:**

- `<owner/repo>` and `<pr>` are required, in that order.
- Flags may interleave between or after the positionals.
- `inspect`, when present, must come before either positional.
  Flags may appear before `inspect`. The parser consumes the
  _first_ `inspect` token (when no positional has yet been
  seen) as the mode subcommand; any later `inspect` token falls
  through to the positional vector. The resulting `UsageError`
  text depends on the positional vector that builds:
  "invalid pull request number: not a number: inspect" when a
  subsequent `inspect` lands in the `<pr>` slot (e.g. `inspect
owner/repo inspect`), "invalid repo slug: missing '/'" when a
  duplicate `inspect` becomes positional[0] before the slug
  (e.g. `inspect inspect 99` — the second `inspect` lands in
  the slug slot), or "expected exactly 2 positionals (owner/repo,
  pr); got <N>" when the total positional count ends up ≠ 2
  (e.g. `owner/repo inspect 99` produces 3 positionals).
- `-h` / `--help` short-circuits all other validation via a
  pre-scan: if either token appears anywhere in the argument
  list, usage is printed to stdout and the process exits 0
  before any other flag is parsed.
- Repeating `--max-iter`, `--status-comment`, or `--trace` is a
  `UsageError`. Repeating `--state-root` is also a `UsageError`.

The `run` script rebuilds the release binary on demand and execs
it. The wrapper invokes `cargo build --release --quiet`, so
successful incremental rebuilds are silent; only warnings and
errors reach stderr from cargo. Stderr emitted **before**
ooda-pr starts (i.e. before any line documented in "Stderr
surface" below) is wrapper / cargo diagnostic noise, not part
of the binary's contract. The binary's own per-iteration logs,
stack note, comment status lines, and final variant block
**are** the contract — see "Stderr surface" for the full
inventory. If the cargo build fails, `run` exits with cargo's
exit code (typically 101 for compile error) and ooda-pr does
not execute — treat such codes as `BinaryError`-equivalent
(see catch-all).

| Flag                | Meaning                                                                                                                                                                                                                                                                                                                                             |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--max-iter N`      | Loop iteration cap. Default 50. Must be ≥1; `--max-iter 0` (or any non-integer / negative) is rejected as `UsageError` regardless of mode (validation runs before mode dispatch). Inspect mode runs exactly once and does not consult the cap value.                                                                                                |
| `--status-comment`  | Post a status comment to the PR each iteration. Deduped per-PR under the always-on state root at `index/pr/<owner>/<repo>/<pr>/status-comment-dedup.json`; the hash input is the renderer's `dedup_key` field, so progress re-posts when the typed rendered state changes.                                                                          |
| `--state-root PATH` | Override the always-on state root. Default resolution is `$OODA_STATE_HOME`, then `$XDG_STATE_HOME/ooda`, then `~/.local/state/ooda`, then `$TMPDIR/ooda`. One root per machine, shared by every OODA agent regardless of domain; domain identity (`pr`, `codex-review`, …) lives in events, not in the path.                                       |
| `--repo-root PATH`  | Target working tree for every `gt` / `git` subprocess. Default: derive from CWD via `git rev-parse --show-toplevel`. Invocations from outside any git tree are rejected as `UsageError` unless `--repo-root` is supplied. Pinning is required so `gt sync` cannot rewrite a sibling repo's stack when the binary is invoked from elsewhere on disk. |
| `--trace PATH`      | Also append the compact trace to PATH. Creates parent directories when needed. Each run appends a run header, binary-owned diagnostic lines, the final Outcome block, and `exit=<code>`. Trace-open failure emits `BinaryError` (exit 70). Later trace writes are best-effort and do not change the Outcome.                                        |
| `-h`, `--help`      | Print usage to stdout, exit 0. The only invocation that writes to stdout. Short-circuits all other validation via a pre-scan: appears anywhere in argv → exit 0 immediately, bypassing the Outcome construction path.                                                                                                                               |

## Always-On State

Every invocation with valid `<owner/repo>` and `<pr>` writes a
host-local audit trail before observation begins. The state model
is domain-agnostic and lives in the `ooda-state` crate; ooda-pr is
one of several writers sharing the same on-disk layout.

The state root resolves via this chain:

1. `--state-root PATH`
2. `$OODA_STATE_HOME`
3. `$XDG_STATE_HOME/ooda`
4. `~/.local/state/ooda`
5. `$TMPDIR/ooda`

One root per machine. Domain identity (`pr` slug, `codex-review`
level, etc.) lives inside event records, not in the path:

```text
<root>/
├── runs/<run-id>/
│   ├── events.jsonl          # source of truth (append-only typed events)
│   └── blobs/<sha>.<ext>     # content-addressed payloads
├── live/<run-id>             # empty marker; presence = active run
└── index/pr/<owner>/<repo>/<pr>/
    └── status-comment-dedup.json   # cross-run dedup memory
```

`<run-id>` is opaque (`<YYYYMMDDTHHMMSSZ>-<entropy>-p<pid>`); the
GitHub slug + PR number live in the `target` payload on the
`run_started` event, not in the path.

**Event vocabulary.** Each line in `events.jsonl` is a typed event
with a `kind` discriminator. Ooda-pr emits:

- `run_started` — first event; `domain: "pr"`, `target` carries
  forge / slug / pr / mode / max_iter / status_comment / cwd / argv.
- `iteration_observed` / `iteration_oriented` — references to a
  `blob` containing the JSON snapshot.
- `iteration_decided` — carries `decision_kind` (e.g.
  `"Execute::Rebase"`, `"Halt::HumanNeeded"`).
- `iteration_executed` / `iteration_waited` — post-act markers.
- `iteration_handoff` — references the handoff prompt blob.
- `run_halted` — terminal event; `outcome` carries the boundary
  variant name, `exit_code` carries the numeric exit. After this
  event the `live/<run-id>` marker is deleted.
- `domain_specific` — catch-all for events the core vocabulary
  doesn't model (observe lifecycle, status-comment lifecycle, raw
  tool-call telemetry, action_started/finished). `kind_suffix`
  names the event; `payload` is opaque JSON.

**Agent entrypoint.** To audit a run: pick the `runs/<run-id>/`
directory (or watch `live/` for active runs), parse `events.jsonl`
line-by-line, and dereference any `blob` field by joining
`runs/<run-id>/blobs/<sha>.<ext>`. Cockpit (`/cockpit`) provides a
live tail; for one-shot audits, walk `runs/` directly.

## Outcomes

Each successful invocation produces exactly one `Outcome` and
emits it as the final stderr block (header + variant payload).
**Dispatch on `$?` alone — no stderr parsing required for
dispatch.** `Handoff*` callers additionally consume the prompt
block (stderr content following the header), and `UsageError`
callers may surface the usage text — but neither parses stderr
to determine _which_ variant fired; that's `$?`.

The `--help` short-circuit is an exception: it exits 0 without
constructing an `Outcome` at all (stdout receives the usage
text; the binary writes nothing to stderr on this path —
though the `run` wrapper may have already emitted cargo
warnings/errors per the wrapper-diagnostics caveat above).

**Stderr surface.** Stderr is divided into a diagnostic prefix
(varies by mode and flags) and a final variant block (the
Outcome's emission). Listed by emission site:

- **Loop mode, per iteration** (interleaved in iteration order):
  - `[iter N] <ActionKind> (<Automation>) blocker: <BlockerKey>` for Execute decisions
    — note the parentheses, distinct from the colon-separated
    `WouldAdvance: <ActionKind>:<Automation>` header form (a
    single regex over both surfaces will mis-parse). Example:
    `[iter 3] WaitForCi (Wait(1m)) blocker: ci_pending: Build`.
  - `[iter N] halt: <DecisionHaltName>` for halts with no action
    payload. For `AgentNeeded` / `HumanNeeded` halts, the line is
    `[iter N] halt: <DecisionHaltName> blocker: <BlockerKey>` (e.g.
    `[iter 5] halt: AgentNeeded blocker: unresolved_threads`).
    `<DecisionHaltName>` is one
    of a finite five-element set of strings (two of which
    contain parentheses, so paren-splitting tokenizers or
    `\w+`-style regexes will split them): `Success`,
    `Terminal(Succeeded)`, `Terminal(Aborted)`, `AgentNeeded`,
    `HumanNeeded`. Each maps to a boundary `Outcome` variant:
    `Success` → `Paused` (exit 1), `Terminal(Succeeded)` →
    `DoneSucceeded` (exit 0, stderr header `DoneMerged`),
    `Terminal(Aborted)` → `DoneAborted` (exit 5, stderr header
    `DoneClosed`), `AgentNeeded` → `HandoffAgent` (exit 4),
    `HumanNeeded` → `HandoffHuman` (exit 3). Payloads are not
    expanded in the iter-log line; the boundary emission carries
    them — `Handoff*` in a content-addressed blob at
    `runs/<run-id>/blobs/<sha>.md` (pointed to by the stderr
    `  see:` line), `Stuck*` in the `:<BlockerKey>` projection,
    terminal/Paused with no payload.
  - When `--status-comment` is set: `[iter N] comment: posted`,
    `[iter N] comment: <PostError>`, or silently skipped on the
    common dedup-no-change case. (See "comment lines" below.)
- **Inspect mode, before the variant block** (at most one each,
  in this order):
  - `stack: <base> → <root>` if the PR's immediate base differs
    from the resolved stack root used for branch-rule lookups.
    Inspect-only by design: one-shot diagnostics get the stack
    note for context; loop mode does not emit it at all (it's
    static for a given PR, and per-iteration repetition would be
    noise).
  - When `--status-comment` is set: `comment: posted`,
    `comment: skipped (unchanged)`, or `comment: <PostError>`.
- **Final variant block** (last emission, both modes): the
  Outcome header, optionally followed by a single pointer line
  `  see: <abs-path-to-handoff-blob>` (`Handoff*`) or the
  usage block (`UsageError`). The path points at a
  content-addressed blob (`runs/<run-id>/blobs/<sha>.md`); the
  prompt body lives in that file, not on stderr — see `Handoff*`
  prompt format below.

The durable audit trail lives in `runs/<run-id>/events.jsonl` —
structured, typed events rather than a free-form text trace.
When `--trace PATH` is set, the binary additionally appends the
compact human-readable trace lines (stack note, iteration log,
comment status lines, final Outcome block, and `exit=<code>`)
to that path as a compatibility sink. Stderr remains the binary
boundary, and `$?` remains the dispatch contract.

**Comment lines** (when `--status-comment` is set):

| Mode    | Posted                     | Dedup skip                     | Error                           |
| ------- | -------------------------- | ------------------------------ | ------------------------------- |
| Inspect | `comment: posted`          | `comment: skipped (unchanged)` | `comment: <PostError>`          |
| Loop    | `[iter N] comment: posted` | (silent — no line)             | `[iter N] comment: <PostError>` |

**Stderr placeholders:**

- `<ActionKind>` — the action's variant name (e.g. `Rebase`,
  `AddressThreads`). Payload-stripped: `WaitForBotReview`, not
  `WaitForBotReview { reviewers: [...] }`. The renderer uses
  `ActionKind::name()`, a hand-written `&'static str` per
  variant.
- `<BlockerKey>` — the action's blocker identifier (defined
  below). The type enforces only non-empty. Construction sites
  interpolate typed payloads (`CheckName`, `GitHubLogin`, etc.)
  into format strings, so values can include any characters
  those types allow — typical values are ASCII-only with colons
  and spaces (e.g. `ci_fail: Build / test`), but unicode is
  possible if upstream payloads carry it. See the `BlockerKey`
  section for sample values and the consequences for parsing
  the `<ActionKind>:<BlockerKey>` projection.
- `<Automation>` — `Full` or `Wait(<duration>)`. The renderer
  has arms for all 4 `Automation` variants, but `decide` routes
  `Agent`/`Human` to halts (`HandoffAgent` / `HandoffHuman`)
  before they could reach a `WouldAdvance`. Only `Full`/`Wait(_)`
  appear here in practice — invariant established at the decide
  boundary, not the render boundary.
  `<duration>` is rendered as `<seconds>s` (under 1 minute),
  `<minutes>m` (whole minutes), or `<minutes>m<seconds>s` for
  the mixed case. Current actions only construct intervals of
  15s, 30s, or 60s — so the surface forms callers will actually
  see are `Wait(15s)`, `Wait(30s)`, and `Wait(1m)`. The
  `<minutes>m<seconds>s` form (e.g. `Wait(1m30s)`) and
  `Wait(0s)` are representable by the formatter but no current
  action constructs them.

**Header format.** The stderr headers with no payload — exactly
`DoneMerged`, `DoneClosed`, `Paused` (underlying variants
`DoneSucceeded`, `DoneAborted`, `Paused`) — emit only the
header token on the line (no colon, no trailing space). All
other variants emit `<Header>: <details>` (colon and one
ASCII space, then payload). A regex matching the header must
allow both forms: `^(DoneMerged|DoneClosed|Paused)$` for the
no-payload headers, `^<Header>: ` for the rest. There is no
`StuckCapReached:` (bare-colon) form — `StuckCapReached`
always carries an `Action` and always emits the
`<ActionKind>:<BlockerKey>` payload.

| Exit | Outcome variant           | Stderr header                                                                | Caller's response                                                                                                                                                                                                                                                                                                                                                                                 |
| :--: | ------------------------- | ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
|  0   | `DoneSucceeded`           | `DoneMerged`                                                                 | Stop. PR merged.                                                                                                                                                                                                                                                                                                                                                                                  |
|  1   | `Paused`                  | `Paused`                                                                     | Stop driving. Internally maps from `DecisionHalt::Success` — per the source comment, "No actions to dispatch, no blockers — PR has reached its target state." The boundary name `Paused` reflects the operational meaning for the caller: stop driving, re-invoke later only if PR state may have changed (e.g., a reviewer acts, CI re-runs, auto-merge fires).                                  |
|  2   | `WouldAdvance(action)`    | `WouldAdvance: <ActionKind>:<Automation>`                                    | **Inspect-only — not a halt.** Re-invoke without `inspect` to drive the action. Do **not** report `WouldAdvance` and stop; that's the most common agent error against this binary. The automation tells you what `act` would do (`Full` runs immediately; `Wait(d)` sleeps then re-observes). See "Driving discipline" for the full anti-pattern list.                                            |
|  3   | `HandoffHuman(action)`    | `Hand off to human: <prompt headline>` (followed by `  see: <path>` pointer) | Read the prompt body from the pointed-to handoff blob (`runs/<run-id>/blobs/<sha>.md`). Surface the handoff to the user (see "Surface to the user" below). Re-invoke `/ooda-pr` after they resolve it.                                                                                                                                                                                            |
|  4   | `HandoffAgent(action)`    | `Hand off to agent: <prompt headline>` (followed by `  see: <path>` pointer) | Read the prompt body from the pointed-to handoff blob (`runs/<run-id>/blobs/<sha>.md`). Surface the handoff to the user (see "Surface to the user" below), then dispatch an agent with the prompt body as input. Re-invoke `/ooda-pr` after the agent finishes.                                                                                                                                   |
|  5   | `DoneAborted`             | `DoneClosed`                                                                 | Stop. PR is closed without merge (e.g., abandoned). Treat per the caller's policy (often: notify owner).                                                                                                                                                                                                                                                                                          |
|  6   | `StuckRepeated(action)`   | `StuckRepeated: <ActionKind>:<BlockerKey>`                                   | Do not auto-retry. Diagnose stderr; fix the underlying issue or escalate.                                                                                                                                                                                                                                                                                                                         |
|  7   | `StuckCapReached(action)` | `StuckCapReached: <ActionKind>:<BlockerKey>`                                 | Re-invoke with a higher `--max-iter`, or escalate. The action shown is the last action `act` ran successfully (Wait or non-Wait). Binary is stateless across runs (except `--status-comment` dedup).                                                                                                                                                                                              |
|  64  | `UsageError(msg)`         | `UsageError: <msg>` (followed by full usage block)                           | Fix the invocation. The usage block (same content as `--help` writes to stdout) is written to stderr immediately after the header, so callers don't need to re-invoke with `--help` to see syntax.                                                                                                                                                                                                |
|  70  | `BinaryError(msg)`        | `BinaryError: <msg>`                                                         | BSD sysexits `EX_SOFTWARE`. Caught external failure (gh subprocess, network, IO). The msg is a single-line human-triage string; do not parse it. Retry once for transient cases or escalate per caller's policy. Distinct from uncaught panics — see catch-all.                                                                                                                                   |
| 130  | `SignalInterrupted`       | `Interrupted: exit code 130`                                                 | `SIGINT` (`128 + 2`). The loop polls `SHUTDOWN_SIGNAL` at each iteration boundary; on a trapped signal it appends a terminal `run_halted` event, releases the live marker, prints the header, and exits 130 itself. Treat as clean shutdown, not a crash. The shell synthesizes the same `128 + 2` for an uncaught signal, so callers cannot distinguish trapped from kernel paths on `$?` alone. |
| 143  | `SignalInterrupted`       | `Interrupted: exit code 143`                                                 | `SIGTERM` (`128 + 15`). Same handling as `SIGINT`; exits 143.                                                                                                                                                                                                                                                                                                                                     |

**1:1 variant-to-exit-code mapping** is the design rule. Each
variant has a unique exit code; `$?` is sufficient for dispatch.

**Exit codes 8–63 and 65–69 are unassigned.** The binary never
emits them in current source. They are held in reserve for
future Outcome variants (additions follow the assigned-table
style: a new variant gets a new code in this range, no code is
ever reused). Codes ≥64 follow BSD `sysexits` starting at
`UsageError = 64`.

### Payload conventions

Each variant carries exactly the evidence its caller needs:

- **`Stuck*`** carries the action whose `(kind, blocker)` pair
  triggered the halt. The `<ActionKind>:<BlockerKey>` projection
  on stderr is informational only — the action is the witness.
  `StuckRepeated` carries the repeated non-Wait action.
  `StuckCapReached` carries the last action `act` ran
  successfully (Wait or non-Wait) — the most recent triage
  anchor when the cap fires.
- **`HandoffAgent` / `HandoffHuman`** are spelled as separate
  variants (rather than `Handoff(Recipient, Action)`) so the
  recipient is observable from the variant name and exit code,
  preserving 1:1 dispatch.
- **`WouldAdvance(Action)`** carries the action; the action's
  `automation` field is rendered on stderr to tell the caller
  what `act` would do. No separate `pace` payload — it lives on
  the action.
- **`BinaryError(String)`** is intentionally opaque at the
  boundary. Loop mode flattens a typed loop-error union (observe
  failure or act failure) into the string; inspect mode can only
  surface observe failures (no act call). **Invariant:** the
  string contains no newlines — any newline in the underlying
  error is replaced with a space at construction, so the stderr
  header always occupies exactly one line.
- **`Paused`** carries no payload. Paused means decide selected
  no candidate action — there is no action to carry. Diagnostic
  context for "why no candidate" lives in the orient log
  (surfaced via `--status-comment`), not in the Outcome.
- **`UsageError(String)`** carries the parser's diagnostic, also
  newline-free.

### Catch-all (uncaught exit codes)

ooda-pr deliberately produces only the exit codes assigned in
the table: {0..=7, 64, 70, 130, 143}. Codes 8–63 and 65–69 are
unassigned and never emitted by current source. Any exit code
outside the assigned set indicates an uncaught binary failure:
typical causes are Rust panics (101), OS signal kills the loop
did not trap (`128 + signal` — e.g. 137 for SIGKILL/OOM, 139
for SIGSEGV), or `run` wrapper failures (cargo build error).

The caller should treat such codes as `BinaryError`-equivalent
for dispatch (alert; do not interpret stderr as a structured
contract — it is a panic message or shell diagnostic, not a
`BinaryError:` header). Retry policy depends on the suspected
cause: panics are deterministic (do not retry), OS signal kills
may be transient (single retry reasonable), build failures
require source repair.

### `Handoff*` prompt format

For `HandoffAgent(action)` and `HandoffHuman(action)`, the prompt
body is **written to disk** as a content-addressed blob at
`<state-root>/runs/<run-id>/blobs/<sha>.md`, and the **only**
stderr emission after the header is a single pointer line of the
form:

```
  see: <absolute-path-to-handoff-blob>
```

The pointer line begins with the literal **7-byte** sequence: two
ASCII spaces (`0x20 0x20`), the ASCII word `see` (`0x73 0x65 0x65`),
an ASCII colon (`0x3a`), and one ASCII space (`0x20`). The
remainder of the line (until `\n`) is the absolute path. There is
no inline prompt body on stderr.

**Caller protocol**: read the path from the pointer line, then
read the file in full. The file is content-addressed (filename is
its SHA-256) and immutable; its size is observable via `stat`, so
callers should `Read` the whole file rather than tail-truncate
stderr (which would lose the dashboard preamble that explains _why_
this action was selected). Do not interpret an `EOF` on stderr
after the pointer line as a boundary on the prompt — the prompt
lives in the file.

The prompt content is non-empty by convention but
`Action.description` is a plain `String` and the type does not
enforce non-emptiness; the file may be zero-length. Embedded
newlines in the description appear verbatim in the handoff blob.

**Surface to the user.** After reading the handoff blob, the
calling agent's next user-visible response MUST expose the handoff
content to the human. At minimum, surface: (1) the final variant
block (stderr header + `  see:` pointer line), (2) the
per-iteration stderr trail emitted during the run (bounded by
`--max-iter`; default cap yields ~50 `[iter N] ...` lines, surface
in full), and (3) the handoff blob body in full. The body already
carries the dashboard preamble (tier-grouped candidates, per-axis
signals, blockers) and the per-action prompt — surfacing the file
verbatim discharges items (2)-(3) for the blockers / signals
portion.

This requirement applies to **both** `HandoffHuman` (exit 3) and
`HandoffAgent` (exit 4) — even when the caller dispatches a
sub-agent, the human must see the dispatch context. The
calling agent MAY add a one-line framing summary above the
surfaced content; it MUST NOT replace the surfaced content with
a summary. Format is the caller's choice (verbatim fenced block,
structured render, collapsible region, etc.) — fidelity is the
constraint, not format.

Rationale: the dashboard preamble + per-action body encode why
this halt fired and what state the PR is in. A summary that omits
them loses the audit surface that lets the human (a) understand
the halt without opening state files, (b) verify the agent's
interpretation before approving the next action, and (c) catch
cases where the agent is about to act on a wrong reading. This is
not in tension with the "do not editorialize" anti-pattern under
**Driving discipline** — that rule forbids agent vibes
displacing the halt-class exit codes; this rule requires the
loop's own reasoning to reach the human.

**Fallback (rare)**: if the recorder cannot write the handoff
blob (IO failure, recorder absent), the binary falls back to the
legacy inline form — a `  prompt: <body>` line (10-byte sentinel
`␣␣prompt:␣`) followed by the prompt content streamed to EOF on
stderr. Callers SHOULD prefer the `see:` form when present; the
fallback exists only so the prompt is never lost. The **Surface
to the user** requirement above applies to the fallback path
verbatim: when the inline form fires, the inline body (the bytes
after the `␣␣prompt:␣` sentinel through EOF) substitutes for the
handoff blob in item (3); items (1)-(2) are unchanged.

In `inspect` mode, the `Handoff*` prompt has the same
**directive form** as in loop mode — the content tells the
recipient what to do, not what would have been requested.

Single-line prompt example:

```
HandoffAgent: Rebase
  see: /home/user/.local/state/ooda/runs/20260516T120000Z-000000000-p1234/blobs/a3f1c2…d4.md
```

Blob contents:

```
Rebase onto the latest base branch
```

Multi-line prompt example. The blob carries the dashboard
preamble + per-action body verbatim:

```
HandoffAgent: AddressThreads
  see: /home/user/.local/state/ooda/runs/20260516T120000Z-000000000-p1234/blobs/b7d2e9…1f.md
```

Blob contents:

```
Recommended (blocking fix): AddressThreads: 2 unresolved [blocker: unresolved_threads]
...
Blockers:
1. unresolved_threads: AddressThreads

Address 2 unresolved review threads.
Copilot: 2 issues.

1. Copilot @ src/foo.rs:42
   > <body>
```

### Mode-restricted variants

`inspect` emits the same `Outcome` variant the loop's first
iteration would produce, with **one substitution rule for the
Execute path**: when `decide` returns `Execute(action)` (i.e.
`automation ∈ {Full, Wait { .. }}`), the loop would dispatch to
`act`; inspect emits `WouldAdvance(action)` instead because
inspect must not act. Actions with `automation ∈ {Agent, Human}`
are halts (not executes) in both modes — inspect emits the same
`HandoffAgent` or `HandoffHuman` variant the loop would.

| Exit | Variant           |                      Loop emits                       |         `inspect` emits         |
| :--: | ----------------- | :---------------------------------------------------: | :-----------------------------: |
|  0   | `DoneSucceeded`   |                          yes                          |               yes               |
|  1   | `Paused`          |                          yes                          |               yes               |
|  2   | `WouldAdvance`    |                          no                           | yes (Execute path substitution) |
|  3   | `HandoffHuman`    |                          yes                          |               yes               |
|  4   | `HandoffAgent`    |                          yes                          |               yes               |
|  5   | `DoneAborted`     |                          yes                          |               yes               |
|  6   | `StuckRepeated`   |                          yes                          | no (requires ≥2 non-Wait iters) |
|  7   | `StuckCapReached` |                          yes                          |     no (cap doesn't apply)      |
|  64  | `UsageError`      | yes (mode-independent — fires before mode dispatched) |     yes (mode-independent)      |
|  70  | `BinaryError`     |                          yes                          |               yes               |

## Loop semantics

Each iteration consists of four stages: `observe → orient →
decide → act`. The first three always run; `act` runs only when
`decide` selects an `Execute(action)` decision (i.e. when
`automation ∈ {Full, Wait { .. }}`). For `Wait` actions, `act`
sleeps the interval and returns; for `Full` actions, `act`
invokes the action's side-effect. For `Agent` / `Human`
automations, `decide` returns a `Halt(...)` directly so `act`
is not called under correct control flow. `act` additionally
returns `ActError::UnsupportedAutomation` as a defense-in-depth
trap for two structural edge cases: (a) an `Agent` or `Human`
action ever reaches `act` (decide should have halted); (b) a
`Full` action with an `ActionKind` not wired into the `Full`
dispatcher. Both are programmer-error traps; neither fires in
practice today.

`observe` runs unconditionally each iteration; `orient` runs whenever `observe` succeeds (an `observe` failure short-circuits to `BinaryError(msg)` exit 70 before `orient`).
`decide` then checks the PR's lifecycle state: if `Merged` or
`Closed`, the loop emits the `DoneSucceeded` or `DoneAborted`
variant respectively (stderr `DoneMerged` / `DoneClosed`;
lifecycle short-circuits inside `decide`, not before `orient`).
Otherwise `decide` selects a candidate action from the `orient`
output and inspects its `automation` field:

| Automation       | `decide` returns            | Loop behavior                                                          |
| ---------------- | --------------------------- | ---------------------------------------------------------------------- |
| `Full`           | `Execute(action)`           | `act` runs the action immediately; loop continues to next iteration    |
| `Wait{interval}` | `Execute(action)`           | `act` sleeps `interval`; loop re-observes (counts toward `--max-iter`) |
| `Agent`          | `Halt(AgentNeeded(action))` | Loop exits with `Outcome::HandoffAgent(action)` (exit 4)               |
| `Human`          | `Halt(HumanNeeded(action))` | Loop exits with `Outcome::HandoffHuman(action)` (exit 3)               |

`automation` is a flat 4-variant enum on `Action`:
`Full | Agent | Wait{interval: Duration} | Human`. There is no
separate `Disposition` type — automation IS the dispatch
selector.

If `decide` selects no candidate (no advancing actions
available), the loop emits `Outcome::Paused` (exit 1).

The loop additionally exits if:

- The same `(kind, blocker)` pair from a non-`Wait` action
  repeats on consecutive non-`Wait` iterations
  (`StuckRepeated(action)` — exit 6). **Stall comparison rule:**
  the comparator's `prev` slot is structurally non-`Wait` (the
  runner only records non-`Wait` actions there). The comparator
  is still invoked for `Wait` current iterations, but a `Wait`
  current never matches a non-`Wait` `prev` (kinds differ), so
  `Wait` iterations are emergent-invisible to stall detection.
  Examples (with `A`, `B` as distinct `(kind, blocker)` pairs,
  `W` for any `Wait`-automation action):
  - `Run(A), W, Run(A)` → trips `StuckRepeated(A)` (W invisible)
  - `Run(A), Run(B), Run(A)` → does not trip (`Run(B)` resets)
  - `W, W, W, ...` → never trips (W never enters comparison)

  **Payload sensitivity.** "Same kind" means full structural
  equality on the `ActionKind` enum value, **including any
  payload fields**. This matters only for kinds that reach the
  comparator — i.e., kinds with `Automation::Full` or
  `Automation::Agent`, since `Wait` iterations are filtered out
  via `last_non_wait` and `Human` iterations halt before
  `act`/the next iteration's comparator. The Agent-automation
  payload-bearing kinds in current source are:
  `AddressThreads { threads }`, `AddressCopilotSuppressed {
count }`, `ShortenTitle { current_len }`, `TriageWait {
blocked_checks }`, `FixCi { check_name }`. If any of these
  payloads mutates iter-to-iter even when the underlying
  blocker is unchanged, kind equality fails and stall does not
  trip — regardless of `BlockerKey`. So `BlockerKey` and the
  kind's payload are two parallel stability axes; payload
  mutation is a second source of stall-detection invisibility
  (alongside `Wait`-automation kinds and `Human`-automation
  halts which never reach the comparator at all).

- The iteration cap is hit (`StuckCapReached(action)` — exit 7).
  The action shown is the last action `act` ran successfully
  (Wait or non-Wait); iterations whose `act` returned an error
  exit with `BinaryError` and never reach `StuckCapReached`.
  `Wait` iterations count toward the cap. The loop is
  iteration-bounded, not wall-clock-bounded; callers needing a
  wall-clock deadline must impose it externally.
- A caught external failure occurs (`BinaryError(msg)` — exit
  70). Internal taxonomy: errors during observe (gh subprocess
  / network / IO) and errors during act dispatch are flattened
  at the boundary into a single human-triage string.

### `BlockerKey`

`BlockerKey` is an opaque human-readable identifier derived
deterministically from the orient observation that prevents the
candidate action from progressing further. Properties:

- **Stable** across iterations for the same underlying blocker
  (so stall detection compares correctly across iterations).
- **Distinct** across distinct blockers (so stall detection does
  not false-trip).
- **Free-form**: source-side validation requires only non-empty.
  Real values include simple tags (`draft`, `wip_label`,
  `merge_conflict`, `unresolved_threads`, `copilot_reviewing`,
  `cursor_reviewing`) and structured tags with embedded
  separators (`ci_fail: Build / test`,
  `pending_bot_review: copilot[bot]`,
  `pending_human_review: alice, bob`,
  `copilot_tier_<slug>`). Both colons and spaces appear in the
  structured forms.
- **Diagnostic only** at the binary boundary. The
  `<ActionKind>:<BlockerKey>` projection on stderr is for human
  triage. Programmatic dispatch must use `$?` alone; do not
  attempt to split on `:` in the stderr header — the
  `<BlockerKey>` field may itself contain colons.

The blocker is carried on every action regardless of automation,
but the stall comparator only sees non-`Wait` actions
(poll-and-repeat is the wait's job).

## Build

Manual build (for development): `cd ~/.claude/skills/ooda-pr && cargo build --release`. The `run` wrapper invokes this on demand for normal use; manual build is useful for warming the cache or driving incremental rebuilds in an IDE. After a manual build, still invoke `run` rather than the binary directly — `run` ensures freshness on subsequent source edits.

For deeper semantics — internal types (`Decision`, `HaltReason`,
`Action`, `Automation`), the orient axes, and named invariants
— see `~/.claude/skills/ooda-pr/README.md`. The contract this
SKILL describes (Outcome variants, exit codes, stderr format) is
self-sufficient for normal caller use.
