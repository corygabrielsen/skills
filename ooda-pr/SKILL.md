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

| Flag                | Meaning                                                                                                                                                                                                                                                                                                     |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--max-iter N`      | Loop iteration cap. Default 50. Must be ≥1; `--max-iter 0` (or any non-integer / negative) is rejected as `UsageError` regardless of mode (validation runs before mode dispatch). Inspect mode runs exactly once and does not consult the cap value.                                                        |
| `--status-comment`  | Post a status comment to the PR each iteration. Deduped per-PR under the always-on state root at `github.com/<owner>/<repo>/prs/<pr>/status-comment/dedup.json`; the hash input is the renderer's `dedup_key` field, so progress re-posts when the typed rendered state changes.                            |
| `--state-root PATH` | Override the always-on state root. Default resolution is `$OODA_PR_STATE_HOME`, then `$XDG_STATE_HOME/ooda-pr`, then `~/.local/state/ooda-pr`, then the platform temp directory. State is keyed by GitHub repo+PR, not by checkout path.                                                                    |
| `--trace PATH`      | Also append the compact trace to PATH. Creates parent directories when needed. Each run appends a run header, binary-owned diagnostic lines, the final Outcome block, and `exit=<code>`. Trace-open failure emits `BinaryError` (exit 6). Later trace writes are best-effort and do not change the Outcome. |
| `-h`, `--help`      | Print usage to stdout, exit 0. The only invocation that writes to stdout. Short-circuits all other validation via a pre-scan: appears anywhere in argv → exit 0 immediately, bypassing the Outcome construction path.                                                                                       |

## Always-On State

Every invocation with valid `<owner/repo>` and `<pr>` writes a local
PR memory harness before observation begins. The default root is:

1. `--state-root PATH`
2. `$OODA_PR_STATE_HOME`
3. `$XDG_STATE_HOME/ooda-pr`
4. `~/.local/state/ooda-pr`
5. the platform temp directory

State is keyed by forge + repo + PR, so the same PR driven from
multiple checkouts shares one host-local memory:

```text
<root>/github.com/<owner>/<repo>/prs/<pr>/
  latest/
    index.md
    state.json
    decision.json
    action.json
    outcome.json
    blockers.md
    next.md
  ledger.md
  ledger.jsonl
  events.jsonl
  status-comment/
    dedup.json
  blobs/
    sha256/<aa>/<bb>/<hash>.zst
  runs/<run-id>/
    manifest.json
    trace.md
    trace.jsonl
    iterations/0001/
      event-range.json
      normalized.json
      oriented.json
      candidates.json
      decision.json
      action.json
      tool-calls/
      act-result.json
```

Agent entrypoint: read `latest/index.md` first, then follow links
to `latest/state.json`, `latest/decision.json`, `ledger.md`, or
`events.jsonl`. Full command stdout/stderr and repeated artifacts
are retained as compressed content-addressed blobs and linked from
events/artifact refs.

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
    `Terminal(Merged)`, `Terminal(Closed)`, `AgentNeeded`,
    `HumanNeeded`. Each maps to a boundary
    `Outcome`: `Success` → `Paused` (exit 7), `Terminal(Merged)`
    → `DoneMerged` (exit 0), `Terminal(Closed)` → `DoneClosed`
    (exit 8), `AgentNeeded` → `HandoffAgent` (exit 5),
    `HumanNeeded` → `HandoffHuman` (exit 3). Payloads are not
    expanded in the iter-log line; the boundary emission carries
    them — `Handoff*` in the prompt block, `Stuck*` in the
    `:<BlockerKey>` projection, terminal/Paused with no payload.
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
  Outcome header, optionally followed by the prompt block
  (`Handoff*`) or the usage block (`UsageError`).

The always-on `runs/<run-id>/trace.md` receives an appended run
header before observation begins, then the same stack / iteration /
comment diagnostic lines that the binary emits during the run, then
the final Outcome block and `exit=<code>`. When `--trace PATH` is
set, the same compact trace is also appended to that path. These
trace files are audit aids only; stderr remains the binary boundary,
and `$?` remains the dispatch contract.

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
  (`format_automation`) has arms for all 4 enum variants, but
  `decide`'s automation classifier (`fn classify` in
  `src/decide.rs:57`) routes `Agent`/`Human` to
  `DecisionHalt::AgentNeeded` / `HumanNeeded`, which `outcome.rs`
  collapses to `HandoffAgent` / `HandoffHuman` Outcome variants
  before they could reach a `WouldAdvance`, so only
  `Full`/`Wait(_)` appear here in practice (decide-side
  invariant, not render-side).
  `<duration>` is rendered as `<seconds>s` (under 1 minute),
  `<minutes>m` (whole minutes), or `<minutes>m<seconds>s` for
  the mixed case. Current actions only construct intervals of
  15s, 30s, or 60s — so the surface forms callers will actually
  see are `Wait(15s)`, `Wait(30s)`, and `Wait(1m)`. The
  `<minutes>m<seconds>s` form (e.g. `Wait(1m30s)`) and
  `Wait(0s)` are representable by the formatter but no current
  action constructs them.

**Header format.** The variants with no payload — exactly
`DoneMerged`, `DoneClosed`, `Paused` — emit only the variant
name on the header line (no colon, no trailing space). All
other variants emit `<Variant>: <details>` (colon and one
ASCII space, then payload). A regex matching the header must
allow both forms: `^(DoneMerged|DoneClosed|Paused)$` for the
no-payload variants, `^<Variant>: ` for the rest. There is no
`StuckCapReached:` (bare-colon) form — `StuckCapReached`
always carries an `Action` and always emits the
`<ActionKind>:<BlockerKey>` payload.

| Exit | Outcome variant           | Stderr header                                           | Caller's response                                                                                                                                                                                                                                                                                                                                                |
| :--: | ------------------------- | ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
|  0   | `DoneMerged`              | `DoneMerged`                                            | Stop. PR merged.                                                                                                                                                                                                                                                                                                                                                 |
|  1   | `StuckRepeated(action)`   | `StuckRepeated: <ActionKind>:<BlockerKey>`              | Do not auto-retry. Diagnose stderr; fix the underlying issue or escalate.                                                                                                                                                                                                                                                                                        |
|  2   | `StuckCapReached(action)` | `StuckCapReached: <ActionKind>:<BlockerKey>`            | Re-invoke with a higher `--max-iter`, or escalate. The action shown is the last action `act` ran successfully (Wait or non-Wait). Binary is stateless across runs (except `--status-comment` dedup).                                                                                                                                                             |
|  3   | `HandoffHuman(action)`    | `HandoffHuman: <ActionKind>` (followed by prompt block) | Surface the prompt verbatim to a human. Re-invoke `/ooda-pr` after they resolve it.                                                                                                                                                                                                                                                                              |
|  4   | `WouldAdvance(action)`    | `WouldAdvance: <ActionKind>:<Automation>`               | **Inspect-only.** Re-invoke without `inspect` to drive the action. The automation tells you what `act` would do (`Full` runs immediately; `Wait(d)` sleeps then re-observes).                                                                                                                                                                                    |
|  5   | `HandoffAgent(action)`    | `HandoffAgent: <ActionKind>` (followed by prompt block) | Dispatch an agent with the prompt as input. Re-invoke `/ooda-pr` after the agent finishes.                                                                                                                                                                                                                                                                       |
|  6   | `BinaryError(msg)`        | `BinaryError: <msg>`                                    | Caught external failure (gh subprocess, network, IO). The msg is a single-line human-triage string; do not parse it. Retry once for transient cases or escalate per caller's policy. Distinct from uncaught panics — see catch-all.                                                                                                                              |
|  7   | `Paused`                  | `Paused`                                                | Stop driving. Internally maps from `DecisionHalt::Success` — per the source comment, "No actions to dispatch, no blockers — PR has reached its target state." The boundary name `Paused` reflects the operational meaning for the caller: stop driving, re-invoke later only if PR state may have changed (e.g., a reviewer acts, CI re-runs, auto-merge fires). |
|  8   | `DoneClosed`              | `DoneClosed`                                            | Stop. PR is closed without merge (e.g., abandoned). Treat per the caller's policy (often: notify owner).                                                                                                                                                                                                                                                         |
|  64  | `UsageError(msg)`         | `UsageError: <msg>` (followed by full usage block)      | Fix the invocation. The usage block (same content as `--help` writes to stdout) is written to stderr immediately after the header, so callers don't need to re-invoke with `--help` to see syntax.                                                                                                                                                               |

**1:1 variant-to-exit-code mapping** is the design rule. Each
variant has a unique exit code; `$?` is sufficient for dispatch.

**Exit codes 9–63 are unassigned.** The binary never emits them
in current source. They are held in reserve for future Outcome
variants (additions follow the assigned-table style: a new
variant gets a new code in this range, no code is ever reused).
Codes ≥64 follow BSD `sysexits` starting at `UsageError = 64`.

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
  boundary. Internal source structure varies by mode: loop mode
  goes through a typed `LoopError = Observe(GhError) |
Act(ActError)` flattened by `From<LoopError>`; inspect mode
  constructs `BinaryError` directly from the `observe` failure
  (no `act` call), so `act:`-prefixed messages cannot occur in
  inspect mode. Either way the string is the flattened
  human-triage rendering. **Invariant:** the string contains no
  newlines (any newline in the underlying error is replaced
  with a space at construction).
- **`Paused`** carries no payload. Paused means decide selected
  no candidate action — there is no action to carry. Diagnostic
  context for "why no candidate" lives in the orient log
  (surfaced via `--status-comment`), not in the Outcome.
- **`UsageError(String)`** carries the parser's diagnostic, also
  newline-free.

### Catch-all (uncaught exit codes)

ooda-pr deliberately produces only the exit codes assigned in
the table: {0, 1, 2, 3, 4, 5, 6, 7, 8, 64}. Codes 9–63 are
reserved for future Outcome variants but never emitted by
current source. Any exit code outside the assigned set
(including the reserved range) indicates an uncaught binary
failure: typical causes are Rust panics (101), OS signal kills
(128 + signal — e.g. 137 for SIGKILL/OOM, 139 for SIGSEGV), or
`run` wrapper failures (cargo build error).

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

The prompt content is non-empty by convention — every current
`decide` site constructs `Action.description` with non-empty
text — but `Action.description` is a plain `String` and the
type does not enforce non-emptiness. Embedded newlines print as-is;
continuation lines appear at column 0 unless the prompt's own
content starts them with whitespace. The prompt block runs from
the first line to **EOF on stderr** (no sentinel; streaming
consumers detect end via process exit). The block always ends
with a trailing `\n` from the renderer's `writeln!`. **Edge
case**: if the description content itself ends in `\n`, the
emitted block ends in `\n\n` (the description's own newline
followed by the writeln-added one); do not interpret consecutive
newlines as a sentinel.

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
Execute path**: when `decide` returns `Execute(action)` (i.e.
`automation ∈ {Full, Wait { .. }}`), the loop would dispatch to
`act`; inspect emits `WouldAdvance(action)` instead because
inspect must not act. Actions with `automation ∈ {Agent, Human}`
are halts (not executes) in both modes — inspect emits the same
`HandoffAgent` or `HandoffHuman` variant the loop would.

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
`decide` selects an `Execute(action)` decision (i.e. when
`automation ∈ {Full, Wait { .. }}`). For `Wait` actions, `act`
performs `thread::sleep(interval)` and returns; for `Full`
actions, `act` invokes the action's side-effect (`gh` call,
etc.). For `Agent` / `Human` automations, `decide` returns a
`Halt(...)` directly so `act` is not called under correct
control flow. As a defense-in-depth guard, `act` itself
returns `ActError::UnsupportedAutomation` for two structural
edge cases: (a) an `Agent` or `Human` action ever reaches `act`
(should not happen — `decide`'s automation classifier (`fn classify` in `src/decide.rs`) halts those before
Execute); (b) a `Full` action with an `ActionKind` not wired
into `act::run_full` (currently impossible — all 3 Full kinds
have arms, but the trap fires if a future `Full` kind is added
without an `act` handler). Both are programmer-error traps;
neither fires in practice today.

`observe` runs unconditionally each iteration; `orient` runs whenever `observe` succeeds (an `observe` failure short-circuits to `BinaryError(msg)` exit 6 before `orient`).
`decide` then checks the PR's lifecycle state: if `Merged` or
`Closed`, the loop emits `DoneMerged` or `DoneClosed` respectively
(lifecycle short-circuits inside `decide`, not before `orient`).
Otherwise `decide` selects a candidate action from the `orient`
output and inspects its `automation` field:

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

- The iteration cap is hit (`StuckCapReached(action)` — exit 2).
  The action shown is the last action `act` ran successfully
  (Wait or non-Wait); iterations whose `act` returned an error
  exit with `BinaryError` and never reach `StuckCapReached`.
  `Wait` iterations count toward the cap. The loop is
  iteration-bounded, not wall-clock-bounded; callers needing a
  wall-clock deadline must impose it externally.
- A caught external failure occurs (`BinaryError(msg)` — exit
  6). Internal taxonomy: `Observe(GhError)` for gh subprocess /
  network / IO during observe; `Act(ActError)` for failures
  during action dispatch. The boundary flattens these into a
  single human-triage string.

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
