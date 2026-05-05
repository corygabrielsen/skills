---
name: ooda-prs
description: Drive N PRs through observe → orient → decide → act in parallel. Each invocation produces one MultiOutcome the caller dispatches on. Aggregate priority projection on `$?`; per-PR records on stdout (JSONL); per-PR variant blocks on stderr.
args:
  - name: <suite>
    description: One or more `<owner/repo>? <pr>+` groups, comma-separated. Required.
  - name: inspect
    description: Optional subcommand. If used, must precede the suite. Runs one observe/orient/decide pass per PR; no act, no loop.
  - name: --max-iter N
    description: Per-PR loop iteration cap. Default 50; must be ≥1. Inspect mode runs exactly one pass per PR.
  - name: --concurrency K
    description: Maximum simultaneously-active PRs (workers). Default = |suite| (no cap). Must be ≥1.
  - name: --status-comment
    description: Post a status comment to each PR every iteration. Per-PR deduped via the always-on state root.
  - name: --state-root PATH
    description: Override the always-on local state root for this invocation.
  - name: --trace PATH
    description: Also append the compact trace to PATH (per-PR appends; lines are not slug-prefixed).
  - name: -h, --help
    description: Print usage to stdout and exit 0. Only invocation that writes structured stdout other than the JSONL stream.
---

# /ooda-prs

Drives **N PRs concurrently** through observe → orient → decide →
act until each halts. The fork-of-`/ooda-pr` (`DRY` not a concern;
the per-PR pipeline is bit-equivalent). Each invocation returns one
`MultiOutcome`; the caller dispatches on the aggregate exit code,
parses the per-PR JSONL records on stdout, and surfaces stderr to
humans for triage.

## Names

| Name        | Refers to                                                                                                                                      |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `/ooda-prs` | The skill (this document).                                                                                                                     |
| `ooda-prs`  | The compiled Rust binary. The `run` wrapper resolves the symlink via `pwd -P` and locates the binary at `target/release/ooda-prs`.             |
| `run`       | The wrapper script at `~/.claude/skills/ooda-prs/run`. Performs the rebuild step (`cargo build --release --quiet`) before exec'ing the binary. |
| Suite       | The non-empty, deduplicated `Vec⟨(RepoSlug, PullRequestNumber)⟩` parsed from the suite grammar. Drives one `run_loop` per pair.                |

Always invoke `run`; never the binary directly.

## Calling discipline

**`$?` MUST reflect ooda-prs's exit when ooda-prs runs.** The same
two concerns from `/ooda-pr` apply verbatim:

1. **ooda-prs must actually run.** `false && ooda-prs ...`
   short-circuits and ooda-prs never executes.
2. **Nothing may inject another exit code into `$?`.** Pipes
   (`ooda-prs | jq`), command substitution (`out=$(ooda-prs ...)`),
   backgrounding (`ooda-prs &`), and any subsequent command
   (`ooda-prs; echo x`) replace `$?`.

**Capturing stdout** (the JSONL records) safely: redirect to a file
(`ooda-prs ... > prs.jsonl`) — file redirection preserves `$?`. The
piping-to-jq pattern requires either a tempfile or the
`PIPESTATUS[0]` (bash) / `pipestatus[1]` (zsh) idiom.

```bash
# Safe: capture stdout, dispatch on $?
~/.claude/skills/ooda-prs/run acme/widget 1 2 > prs.jsonl
case $? in
  0)  echo "all done" ;;
  5)  jq -r 'select(.outcome=="HandoffAgent") | .prompt' prs.jsonl | dispatch_agents ;;
  3)  jq -r 'select(.outcome=="HandoffHuman")' prs.jsonl | notify_human ;;
  1|2|6) jq . prs.jsonl >&2; escalate ;;
  64) echo "fix invocation" >&2 ;;
esac
```

## Suite grammar

```
<args>      ::= <flag>* <group> ( ',' <group> )* <flag>*
<group>     ::= <slug>? <pr>+
<slug>      ::= <owner> '/' <repo>
<pr>        ::= positive_integer
```

**Slug resolution:**

- A `<slug>` is detected by the presence of `'/'` in a positional
  token. Tokens without `/` are PR numbers.
- A group with **no explicit slug** inherits the prior group's
  slug. The very first group, if it has no explicit slug, falls
  back to inferring the cwd's repository via `gh repo view --json
nameWithOwner`.
- Cwd inference failure (no `gh`, not a github repo, malformed
  output) is a `UsageError` with the diagnostic.

**Distinct elements:** the parser rejects duplicate `(slug, pr)`
pairs as `UsageError("duplicate PR: <slug>#<pr>")`. Comma-separated
groups must be non-empty (lone `,,` rejects).

| Input                                        | Parsed suite                                                |
| -------------------------------------------- | ----------------------------------------------------------- |
| `ooda-prs 42 45`                             | `[(cwd, 42), (cwd, 45)]`                                    |
| `ooda-prs 765 777 983`                       | `[(cwd, 765), (cwd, 777), (cwd, 983)]`                      |
| `ooda-prs acme/widget 42, acme/infra 100`    | `[(acme/widget, 42), (acme/infra, 100)]`                    |
| `ooda-prs acme/widget 42 43, acme/infra 100` | `[(acme/widget, 42), (acme/widget, 43), (acme/infra, 100)]` |
| `ooda-prs acme/widget 42, 43`                | `[(acme/widget, 42), (acme/widget, 43)]` (slug inheritance) |
| `ooda-prs 42, acme/infra 100`                | `[(cwd, 42), (acme/infra, 100)]`                            |

## How to call

```bash
~/.claude/skills/ooda-prs/run [options] <suite>            # loop mode
~/.claude/skills/ooda-prs/run inspect [options] <suite>    # one pass per PR
```

| Flag                | Meaning                                                                                                  |
| ------------------- | -------------------------------------------------------------------------------------------------------- | ----- | ------------------------ |
| `--max-iter N`      | **Per-PR** iteration cap. Default 50. Must be ≥ 1. Inspect runs once per PR; cap unused.                 |
| `--concurrency K`   | Maximum simultaneously-active PRs. Default = `                                                           | suite | ` (no cap). Must be ≥ 1. |
| `--status-comment`  | Post status comments to each PR every iteration. Per-PR dedup at `…/prs/<pr>/status-comment/dedup.json`. |
| `--state-root PATH` | Override the always-on state root. Same default-resolution chain as `/ooda-pr`.                          |
| `--trace PATH`      | Also append the compact trace to PATH (each PR appends independently).                                   |
| `-h`, `--help`      | Print usage to stdout, exit 0. Pre-scan short-circuits all other validation.                             |

**Repeating a flag** (`--max-iter`, `--concurrency`,
`--status-comment`, `--state-root`, `--trace`) is a `UsageError`.

## Always-on state

Each invocation writes both per-PR and suite-level audit trails.

### Per-PR (shared with `/ooda-pr`)

Same layout as `/ooda-pr`. Keyed by forge + repo + PR, so the same
PR driven from `/ooda-pr` and `/ooda-prs` shares one host-local
memory.

```text
<root>/github.com/<owner>/<repo>/prs/<pr>/
  latest/        index.md, state.json, decision.json, action.json,
                 outcome.json, blockers.md, next.md
  ledger.{md,jsonl}                                        # cross-run causality
  events.jsonl                                             # all events, all runs
  status-comment/dedup.json
  blobs/sha256/<aa>/<bb>/<hash>.zst
  runs/<run-id>/manifest.json, trace.{md,jsonl},
                iterations/0001/{state,decision,...}.json,
                tool-calls/...
```

### Suite-level (new in `/ooda-prs`)

```text
<root>/suites/<suite-id>/
  manifest.json    -- argv, started_at, suite, mode, max_iter,
                      status_comment, concurrency, cwd
  pointers.json    -- per-PR (slug, pr) → run_id cross-references
  outcome.json     -- the final MultiOutcome + aggregate exit code
  trace.md         -- human-readable summary table
```

`<suite-id>` shares the same `<utc>-<nanos>-p<pid>` shape as per-PR
`<run-id>`. Two simultaneous suite invocations against overlapping
PRs each get a distinct `<suite-id>`; the per-PR `runs/<run-id>/`
namespacing prevents ledger-level collisions.

## MultiOutcome

```
MultiOutcome =
    UsageError(String)                                 -- parser failure; no PRs ran
  ⊕ Bundle(Vec⟨ProcessOutcome⟩)                        -- every PR reached a halt state

ProcessOutcome = (RepoSlug, PullRequestNumber, Outcome)

Outcome =                                              -- per-PR; identical to /ooda-pr
    DoneMerged | StuckRepeated(Action) | StuckCapReached(Action)
  ⊕ HandoffHuman(Action) | WouldAdvance(Action) | HandoffAgent(Action)
  ⊕ BinaryError(String) | Paused | DoneClosed | UsageError(String)
```

Each PR's `Outcome` is bit-equivalent to running `/ooda-pr` on that
PR alone. The suite boundary lifts these per-PR Outcomes to a
single binary boundary for shell dispatch.

### Aggregate exit code (`$?`)

`MultiOutcome::exit_code()` is a **priority projection**:

| Condition                                         | `$?` |
| :------------------------------------------------ | :--: |
| `MultiOutcome::UsageError`                        |  64  |
| any `ProcessOutcome` carries `BinaryError(_)`     |  6   |
| else any carries `HandoffAgent(_)`                |  5   |
| else any carries `HandoffHuman(_)`                |  3   |
| else any carries `StuckCapReached(_)`             |  2   |
| else any carries `StuckRepeated(_)`               |  1   |
| else any carries `WouldAdvance(_)` (inspect-only) |  4   |
| else (all `DoneMerged` / `DoneClosed` / `Paused`) |  0   |

**Priority order** (highest first): `UsageError > BinaryError >
HandoffAgent > HandoffHuman > StuckCapReached > StuckRepeated >
WouldAdvance > terminal`. Per-PR `Paused` (single-PR exit 7) and
`DoneClosed` (single-PR exit 8) collapse to `0` at the suite level —
they are non-actionable terminal states. Per-PR records on stdout
disambiguate.

This contract is **coarser** than `/ooda-pr`'s 1:1 variant→exit
mapping, by design: shell dispatch on `$?` is single-byte, while
the harness needs _coarse_ dispatch ("any agent work? any errors?").
The fine-grained per-PR records belong on stdout.

## Output channels

Three channels, structurally distinct:

### Stdout — JSONL records (the agent-harness contract)

After all PRs halt, one record per PR is emitted to stdout in
**input order**. Each record is a single line of JSON:

```jsonl
{"slug":"acme/widget","pr":1,"outcome":"DoneMerged","exit":0}
{"slug":"acme/widget","pr":2,"outcome":"HandoffAgent","exit":5,"action":"AddressThreads","blocker":"unresolved_threads","prompt":"Address 2 unresolved review threads.\n\n1. Copilot @ src/foo.rs:42\n   > <body>"}
{"slug":"acme/infra","pr":100,"outcome":"WouldAdvance","exit":4,"action":"WaitForCi","blocker":"ci_pending: build","automation":"Wait(1m)"}
```

Schema:

| Field        | Type    | Always present? | Notes                                                                                                         |
| ------------ | ------- | :-------------: | ------------------------------------------------------------------------------------------------------------- |
| `slug`       | string  |       yes       | `<owner>/<repo>`                                                                                              |
| `pr`         | integer |       yes       | positive integer                                                                                              |
| `outcome`    | string  |       yes       | variant name (`DoneMerged`, `HandoffAgent`, …); 10 possible values                                            |
| `exit`       | integer |       yes       | per-PR exit code per `/ooda-pr`'s 1:1 mapping (`0`, `1`, `2`, `3`, `4`, `5`, `6`, `7`, `8`, or `64`)          |
| `action`     | string  |   conditional   | `Stuck*`, `Handoff*`, `WouldAdvance` — the `ActionKind::name()` (e.g. `"Rebase"`, `"AddressThreads"`)         |
| `blocker`    | string  |   conditional   | same condition as `action` — the `BlockerKey` (free-form ASCII, may contain `:` and spaces)                   |
| `prompt`     | string  |   conditional   | `Handoff*` only — verbatim agent/human prompt from `Action.description`. May contain newlines (encoded `\n`). |
| `automation` | string  |   conditional   | `WouldAdvance` only — `"Full"`, `"Wait(15s)"`, `"Wait(30s)"`, `"Wait(1m)"`, etc.                              |
| `msg`        | string  |   conditional   | `BinaryError` only — single-line human-triage string (newlines flattened to spaces by the binary)             |

`UsageError` (parse failure) emits **no stdout** — `$? = 64` and
the stderr usage block are sufficient. The JSONL stream is a clean
"every PR ran" contract.

### Stderr — interleaved iteration logs + per-PR variant blocks

Stderr carries the same per-iteration logs as `/ooda-pr`
(`[iter N] <ActionKind> (<Automation>) blocker: <BlockerKey>` for
Execute decisions; `[iter N] halt: <DecisionHaltName>` for halts)
plus the per-PR final variant block for each PR. With concurrent
threads, lines from different PRs can interleave; per-line
atomicity (one full line per `eprintln!`) is preserved by the OS.

### `$?` — aggregate dispatch

The single-byte coarse dispatch (see table above).

## Harness pattern

```
loop:
  ./run <suite> > prs.jsonl
  case $? in
    0)  break ;;                                # all done
    5)  for prompt in $(jq … prs.jsonl); do
          dispatch_subagent <<< "$prompt" &     # parallel agent dispatches
        done; wait
        ;;                                      # then re-invoke
    3)  notify_human < prs.jsonl; break ;;      # waiting on human
    1|2) escalate < prs.jsonl ;;                # stuck — diagnose
    6)  triage_error < prs.jsonl ;;             # observe/act error
    64) fix_invocation ;;                       # parser error
  esac
```

The single-PR pattern from `/ooda-pr` (one Outcome → one
dispatch → re-invoke) generalizes cleanly: the harness now
dispatches **N parallel sub-agents** when `$? = 5`, then re-invokes
`/ooda-prs`. This is the leverage of the multi-PR mode — `N`
agents work in parallel rather than serializing through one
parent thread.

## Loop semantics

Per-PR, identical to `/ooda-pr`:

- Each iteration runs `observe → orient → decide → act`.
- `act` runs only for `Execute(action)` decisions
  (`automation ∈ {Full, Wait}`); `Agent` / `Human` automations are
  halts.
- Stall detection on `(kind, blocker)` of consecutive non-Wait
  actions.
- Iteration cap (`--max-iter`) per-PR.

Across PRs:

- Each PR runs its own `run_loop` on its own thread.
- Threads are spawned under `std::thread::scope`; the binary exits
  only when all PR threads have joined.
- A `HandoffAgent` for PR_i halts only PR_i — sibling PRs continue.
  This is the source of the multi-PR leverage.
- The aggregate Outcome reflects every PR's final state;
  re-invocation drives the next round.

## Per-PR ↔ suite invariants

Internal invariants of the design:

```
[P1] ∀ PR (slug, pr) ∈ suite, the trajectory of run_loop(slug, pr, …)
     inside ooda-prs is observationally indistinguishable from
     ooda-pr on the same (slug, pr). (Per-PR semantic preservation.)

[P2] ∀ distinct PR_i, PR_j ∈ suite, the action stream of PR_i is
     causally independent of PR_j's. (No shared mutable state.)

[P3] ooda-prs always terminates if every per-PR run_loop terminates.
     (Bounded by `--max-iter` per PR.)

[P4] MultiOutcome::exit_code is total. (See `multi_outcome.rs`.)

[P5] CLI parser is total over Argv: every input → either valid
     Suite or UsageError(_).

[P6] Recorder soundness: per-PR Recorder is single-writer (one
     thread); suite Recorder writes occur from the main thread
     before/after the spawn loop join.

[P7] The harness pattern composes: `$? ∈ {3, 5}` ⇒ caller has
     actionable work; `$? = 0` ⇒ converged; otherwise escalate.

[P8] No surviving counterexample. (See `README.md` for the
     enumerated counterexample sweep.)
```

## Build

Manual build (for development): `cd ~/.claude/skills/ooda-prs &&
cargo build --release`. The `run` wrapper invokes this on demand
for normal use.

For deeper semantics — internal types (`Decision`, `HaltReason`,
`Action`, `Automation`, `MultiOutcome`, `ProcessOutcome`), the
orient axes, and the suite spawn loop's atomic-counter rolling
concurrency — see `~/.claude/skills/ooda-prs/README.md`. The
contract this SKILL describes (suite grammar, MultiOutcome,
aggregate exit code, stdout JSONL) is self-sufficient for normal
caller use.
