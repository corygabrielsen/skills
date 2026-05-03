---
name: ooda-pr
description: Drive a PR through observe → orient → decide → act. Each invocation produces exactly one Outcome the caller dispatches on. 1:1 variant-to-exit-code; dispatch on `$?` alone.
args:
  - name: <owner/repo>
    description: GitHub repository slug. Required positional.
  - name: <pr>
    description: PR number. Required positional.
  - name: inspect
    description: Optional subcommand. If used, must be the FIRST argument after the binary name. Runs one observe/orient/decide pass; no act, no loop.
  - name: --max-iter N
    description: Loop iteration cap. Default 50; must be ≥1. Rejected as UsageError otherwise. Inspect mode runs exactly one pass and ignores this flag.
  - name: --status-comment
    description: Post a status comment to the PR each iteration. Deduped per-PR via a host-local hash file.
  - name: -h, --help
    description: Print usage to stdout and exit 0. The only invocation that writes to stdout.
---

# /ooda-pr

Drives one PR through observe → orient → decide → act. Each
invocation returns one `Outcome`; the caller dispatches on the
exit code alone.

## Names

| Name       | Refers to                                                             |
| ---------- | --------------------------------------------------------------------- |
| `/ooda-pr` | The skill (this document, invoked from a Claude Code agent prompt).   |
| `ooda-pr`  | The Rust binary at `~/.claude/skills/ooda-pr/target/release/ooda-pr`. |
| `run`      | The wrapper script at `~/.claude/skills/ooda-pr/run`.                 |

Always invoke `run`; never the binary directly (the binary path
can serve a stale build silently after source edits).

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
- ooda-pr on the right of `&&`: `~/.claude/skills/ooda-pr/run owner/repo 42` after a reliably-succeeding left side

**Capturing stderr:** redirect to a file (`ooda-pr ... 2>file`).
File redirection does not affect `$?`. Process substitution
(`ooda-pr 2> >(tee file)`) also preserves `$?`. The forms that
break `$?` are stderr **piping** (`|&`, `2>&1 |`).

**Separate Bash tool calls in the same agent turn are fine** —
each Bash call is an independent shell with its own `$?`.

## How to call

```bash
~/.claude/skills/ooda-pr/run [options] <owner/repo> <pr>           # loop mode
~/.claude/skills/ooda-pr/run inspect [options] <owner/repo> <pr>   # one pass
```

**Argument rules:**

- `<owner/repo>` and `<pr>` are required, in that order.
- Flags may interleave between or after the positionals.
- `inspect`, when present, is the FIRST argument (before any flag
  or positional). Any other position is a `UsageError`.
- `-h` / `--help` short-circuits all other validation: prints
  usage to stdout and exits 0. May appear in any position,
  including before `inspect`.
- Repeating any flag is a `UsageError`.

The `run` script rebuilds the release binary on demand and execs
it. If the cargo build fails, `run` exits with cargo's exit code
(typically 101 for compile error) and ooda-pr does not execute —
treat such codes as `BinaryError`-equivalent (see catch-all).

| Flag               | Meaning                                                                                                                                                                                                                                                                                     |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--max-iter N`     | Loop iteration cap. Default 50. Must be ≥1; otherwise rejected as `UsageError`. Inspect mode ignores this flag (runs exactly once).                                                                                                                                                         |
| `--status-comment` | Post a status comment to the PR each iteration. Deduped per-PR via a host-local hash file at `/tmp/ooda-pr/<owner>/<repo>/<pr>.hash`; identical-body re-posts are suppressed across runs until `/tmp` is cleared (lost on reboot, on tmpfs evictions, or when running on a different host). |
| `-h`, `--help`     | Print usage to stdout, exit 0. The only invocation that writes to stdout.                                                                                                                                                                                                                   |

## Outcomes

Every invocation produces exactly one `Outcome`. **Dispatch on `$?`
alone — no stderr parsing required.** Stderr carries a single-line
header for diagnostic purposes and (for `Handoff*` variants) the
verbatim prompt material. Nothing follows the header (or, for
`Handoff*`, the prompt block) on stderr.

**Stderr placeholders:**

- `<ActionKind>` — the action's variant name (e.g. `Rebase`,
  `AddressThreads`).
- `<BlockerKey>` — the action's stable blocker identifier
  (defined below). ASCII-safe, no colons.
- `<Automation>` — the action's automation: `Full`, `Wait(<duration>)`
  where `<duration>` is `humantime`-formatted (e.g. `Wait(30s)`,
  `Wait(1m30s)`), `Agent`, or `Human`.

**Single header format** (all variants): `<Variant>: <details>`
where `<details>` may be empty.

| Exit | Outcome variant         | Stderr header                                                      | Caller's response                                                                                                                                                                                                                   |
| :--: | ----------------------- | ------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
|  0   | `DoneMerged`            | `DoneMerged`                                                       | Stop. PR merged.                                                                                                                                                                                                                    |
|  1   | `StuckRepeated(action)` | `StuckRepeated: <ActionKind>:<BlockerKey>`                         | Do not auto-retry. Diagnose stderr; fix the underlying issue or escalate.                                                                                                                                                           |
|  2   | `StuckCapReached(opt)`  | `StuckCapReached: <ActionKind>:<BlockerKey>` or `StuckCapReached:` | Re-invoke with a higher `--max-iter`, or escalate. Last-attempted action shown when present (cap can hit on iter 1 with no acts → bare header). Binary is stateless across runs (except `--status-comment` dedup).                  |
|  3   | `HandoffHuman(action)`  | `HandoffHuman: <ActionKind>` (followed by prompt block)            | Surface the prompt verbatim to a human. Re-invoke `/ooda-pr` after they resolve it.                                                                                                                                                 |
|  4   | `WouldAdvance(action)`  | `WouldAdvance: <ActionKind>:<Automation>`                          | **Inspect-only.** Re-invoke without `inspect` to drive the action. The automation tells you what `act` would do (`Full` runs immediately; `Wait(d)` sleeps then re-observes).                                                       |
|  5   | `HandoffAgent(action)`  | `HandoffAgent: <ActionKind>` (followed by prompt block)            | Dispatch an agent with the prompt as input. Re-invoke `/ooda-pr` after the agent finishes.                                                                                                                                          |
|  6   | `BinaryError(msg)`      | `BinaryError: <msg>`                                               | Caught external failure (gh subprocess, network, IO). The msg is a single-line human-triage string; do not parse it. Retry once for transient cases or escalate per caller's policy. Distinct from uncaught panics — see catch-all. |
|  7   | `Paused`                | `Paused`                                                           | Stop driving. PR is open with no advancing actions found this pass. May re-invoke later if PR state may have changed.                                                                                                               |
|  8   | `DoneClosed`            | `DoneClosed`                                                       | Stop. PR is closed without merge (e.g., abandoned). Treat per the caller's policy (often: notify owner).                                                                                                                            |
|  64  | `UsageError(msg)`       | `UsageError: <msg>`                                                | Fix the invocation. `--help` shows the usage.                                                                                                                                                                                       |

**1:1 variant-to-exit-code mapping** is the design rule. Each
variant has a unique exit code; `$?` is sufficient for dispatch.

**Exit codes 9–63** are not currently emitted. They are reserved
for future Outcome variants. Codes ≥64 follow BSD `sysexits`
starting at `UsageError = 64`.

### Payload conventions

Each variant carries exactly the evidence its caller needs:

- **`Stuck*`** carries the action whose `(kind, blocker)` pair
  triggered the halt. The `<ActionKind>:<BlockerKey>` projection
  on stderr is informational only — the action is the witness.
  `StuckCapReached` carries `Option<Action>` because the cap can
  trip before any action executes (iteration 1 with `decide`
  selecting a candidate but the cap being 1).
- **`HandoffAgent` / `HandoffHuman`** are spelled as separate
  variants (rather than `Handoff(Recipient, Action)`) so the
  recipient is observable from the variant name and exit code,
  preserving 1:1 dispatch.
- **`WouldAdvance(Action)`** carries the action; the action's
  `automation` field is rendered on stderr to tell the caller
  what `act` would do. No separate `pace` payload — it lives on
  the action.
- **`BinaryError(String)`** is intentionally opaque at the
  boundary. Internally the binary uses typed errors
  (`Observe(GhError)` / `Act(ActError)`); the string is the
  flattened human-triage rendering. **Invariant:** the string
  contains no newlines (any newline in the underlying error is
  replaced with a space at construction).
- **`Paused`** carries no payload. Paused means decide selected
  no candidate action — there is no action to carry. Diagnostic
  context for "why no candidate" lives in the orient log
  (surfaced via `--status-comment`), not in the Outcome.
- **`UsageError(String)`** carries the parser's diagnostic, also
  newline-free.

### Catch-all (uncaught exit codes)

ooda-pr deliberately produces only the exit codes assigned in
the table ({0, 1, 2, 3, 4, 5, 6, 7, 8, 64}) plus the reserved
range 9–63 (currently unused). Any other exit code indicates an
uncaught binary failure: typical causes are Rust panics (101),
OS signal kills (128 + signal — e.g. 137 for SIGKILL/OOM, 139
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
is verbatim prompt material — surface it as-is to the recipient,
do not paraphrase. The prompt block begins on the line immediately
after the header. Its first line begins with the literal **10-byte**
sequence consisting of two ASCII spaces (`0x20 0x20`), the ASCII
word `prompt` (`0x70 0x72 0x6f 0x6d 0x70 0x74`), an ASCII colon
(`0x3a`), and one ASCII space (`0x20`). Strip exactly those 10
bytes from the **first line only**; the byte at offset 10 is the
start of the prompt content. **Continuation lines are unprefixed**
— do not strip from them.

The prompt content is non-empty. Embedded newlines print as-is;
continuation lines appear at column 0 unless the prompt's own
content starts them with whitespace. The prompt block runs from
the first line to **EOF on stderr** (no sentinel; streaming
consumers detect end via process exit). Whether the last byte is
`\n` is content-dependent — strip no trailing whitespace.

In `inspect` mode, the `Handoff*` prompt has the same
**directive form** as in loop mode — the content tells the
recipient what to do, not what would have been requested.

Single-line prompt example:

```
HandoffAgent: Rebase
  prompt: Rebase onto the latest base branch
```

Multi-line prompt example. **The blank line and the leading
whitespace on `   > <body>` are part of the prompt content,
not added by ooda-pr:**

```
HandoffAgent: AddressThreads
  prompt: Address 2 unresolved review threads.
Copilot: 2 issues.

1. Copilot @ src/foo.rs:42
   > <body>
```

### Mode-restricted variants

`inspect` emits the same `Outcome` variant the loop's first
iteration would produce, with **one substitution rule for the
Execute path**: when `decide` selects an action with
`automation ∈ {Full, Wait(_)}`, the loop wraps it in
`Decision::Execute` (which `act` would dispatch); inspect emits
`WouldAdvance(action)` instead because inspect must not act.
Actions with `automation ∈ {Agent, Human}` are halts (not
executes) in both modes — inspect emits the same `HandoffAgent`
or `HandoffHuman` variant the loop would.

| Exit | Variant           |                      Loop emits                       |         `inspect` emits         |
| :--: | ----------------- | :---------------------------------------------------: | :-----------------------------: |
|  0   | `DoneMerged`      |                          yes                          |               yes               |
|  1   | `StuckRepeated`   |                          yes                          | no (requires ≥2 non-Wait iters) |
|  2   | `StuckCapReached` |                          yes                          |     no (cap doesn't apply)      |
|  3   | `HandoffHuman`    |                          yes                          |               yes               |
|  4   | `WouldAdvance`    |                          no                           | yes (Execute path substitution) |
|  5   | `HandoffAgent`    |                          yes                          |               yes               |
|  6   | `BinaryError`     |                          yes                          |               yes               |
|  7   | `Paused`          |                          yes                          |               yes               |
|  8   | `DoneClosed`      |                          yes                          |               yes               |
|  64  | `UsageError`      | yes (mode-independent — fires before mode dispatched) |     yes (mode-independent)      |

## Loop semantics

Each iteration consists of four stages: `observe → orient →
decide → act`. The first three always run; `act` runs only when
`decide` selects an `Execute(action)` decision.

If `observe` detects the PR's lifecycle state is `Merged` or
`Closed`, the loop emits `DoneMerged` or `DoneClosed` respectively
(lifecycle short-circuits before `orient`). Otherwise `decide`
selects a candidate action from the `orient` output and inspects
its `automation` field:

| Automation       | `decide` returns            | Loop behavior                                                          |
| ---------------- | --------------------------- | ---------------------------------------------------------------------- |
| `Full`           | `Execute(action)`           | `act` runs the action immediately; loop continues to next iteration    |
| `Wait{interval}` | `Execute(action)`           | `act` sleeps `interval`; loop re-observes (counts toward `--max-iter`) |
| `Agent`          | `Halt(AgentNeeded(action))` | Loop exits with `Outcome::HandoffAgent(action)` (exit 5)               |
| `Human`          | `Halt(HumanNeeded(action))` | Loop exits with `Outcome::HandoffHuman(action)` (exit 3)               |

`automation` is a flat 4-variant enum on `Action`:
`Full | Agent | Wait{interval: Duration} | Human`. There is no
separate `Disposition` type — automation IS the dispatch
selector.

If `decide` selects no candidate (no advancing actions
available), the loop emits `Outcome::Paused` (exit 7).

The loop additionally exits if:

- The same `(kind, blocker)` pair from a non-`Wait` action
  repeats on consecutive non-`Wait` iterations
  (`StuckRepeated(action)` — exit 1). **Stall comparison rule:**
  the comparator advances only on non-`Wait` iterations; `Wait`
  iterations are skipped. Examples (with `A`, `B` as distinct
  `(kind, blocker)` pairs, `W` for any `Wait`-automation action):
  - `Run(A), W, Run(A)` → trips `StuckRepeated(A)` (W is skipped)
  - `Run(A), Run(B), Run(A)` → does not trip (`Run(B)` resets)
  - `W, W, W, ...` → never trips (W never enters comparison)
- The iteration cap is hit (`StuckCapReached(opt)` — exit 2).
  `opt` is `Some(action)` when at least one action ran (the most
  recent), `None` when no action ran (cap=1 with `decide`
  returning `Execute` but the act not yet performed when cap
  trips). `Wait` iterations count toward the cap. The loop is
  iteration-bounded, not wall-clock-bounded; callers needing a
  wall-clock deadline must impose it externally.
- A caught external failure occurs (`BinaryError(msg)` — exit
  6). Internal taxonomy: `Observe(GhError)` for gh subprocess /
  network / IO during observe; `Act(ActError)` for failures
  during action dispatch. The boundary flattens these into a
  single human-triage string.

### `BlockerKey`

`BlockerKey` is an opaque identifier derived deterministically
from the orient observation that prevents the candidate action
from progressing further. Properties:

- **Stable** across iterations for the same underlying blocker
  (so stall detection compares correctly across iterations).
- **Distinct** across distinct blockers (so stall detection does
  not false-trip).
- **ASCII-safe**: matches `[A-Za-z0-9_-]+`. **No colons** —
  the stderr `<ActionKind>:<BlockerKey>` separator depends on
  this invariant.
- **Diagnostic only** at the binary boundary. Callers do not
  parse it; it appears on stderr to aid human triage.

For `Wait` actions the blocker is the wait condition (e.g.
`WaitingForCi`, `WaitingForReview`). The blocker is carried on
every action regardless of automation, but the stall comparator
ignores `Wait` actions (poll-and-repeat is their job).

## Build

Manual build (for development): `cd ~/.claude/skills/ooda-pr && cargo build --release`. The `run` wrapper invokes this on demand for normal use; manual build is useful for warming the cache or driving incremental rebuilds in an IDE. After a manual build, still invoke `run` rather than the binary directly — `run` ensures freshness on subsequent source edits.

For deeper semantics — internal types (`Decision`, `HaltReason`,
`Action`, `Automation`), the orient axes, and named invariants
— see `~/.claude/skills/ooda-pr/README.md`. The contract this
SKILL describes (Outcome variants, exit codes, stderr format) is
self-sufficient for normal caller use.
