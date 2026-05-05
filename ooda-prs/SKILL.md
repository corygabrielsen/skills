---
name: ooda-prs
description: Drive N PRs through observe → orient → decide → act in parallel. Each invocation produces one MultiOutcome the caller dispatches on. Aggregate priority projection on `$?`; per-PR records on stdout (JSONL); per-PR variant blocks on stderr.
args:
  - name: <suite>
    description: One or more `<owner/repo>? <pr>+` groups, comma-separated. Required. (See "Suite grammar" in the body for the full BNF.)
  - name: inspect
    description: Optional subcommand. Must appear before the first positional token; flags may interleave freely. Runs one observe/orient/decide pass per PR; no act, no loop.
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
act until each halts. `/ooda-prs` is a fork of `/ooda-pr`; the
per-PR pipeline is bit-equivalent (DRY-shared code is intentionally
absent — the fork is independent). Each invocation returns one
`MultiOutcome`; the caller dispatches on the aggregate exit code,
parses the per-PR JSONL records on stdout, and surfaces stderr to
humans for triage.

## Names

| Name        | Refers to                                                                                                                                                                                                                                                                                                                       |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/ooda-prs` | The skill (this document).                                                                                                                                                                                                                                                                                                      |
| `ooda-prs`  | The compiled Rust binary at `target/release/ooda-prs`, sibling to this `SKILL.md` in the source tree.                                                                                                                                                                                                                           |
| `run`       | The wrapper script at `~/.claude/skills/ooda-prs/run`. Inside a `(cd … && cargo build --release --quiet) >&2` subshell it builds the binary (cargo no-ops when already up-to-date), then `exec`s the absolute binary path with the user's cwd unchanged — required so cwd-slug inference (`gh repo view`) sees the user's repo. |
| Suite       | The non-empty, distinct `Vec⟨(RepoSlug, PullRequestNumber)⟩` parsed from the suite grammar (duplicates are rejected as `UsageError`, not silently de-duplicated). Drives `run_loop` per pair in loop mode; drives `run_inspect` per pair in inspect mode.                                                                       |
| `/ooda-pr`  | The single-PR sibling skill, installed at `~/.claude/skills/ooda-pr/`. The fork shares the on-disk state schema (state-root chain, per-PR layout) but does not depend on `/ooda-pr` being installed at runtime — the per-PR pipeline is a complete duplicate inside `/ooda-prs`.                                                |

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
# Capture stdout to a file (preserves $?), then dispatch on $?.
~/.claude/skills/ooda-prs/run acme/widget 1 2 > prs.jsonl
case $? in
  0)     echo "all done" ;;
  5)     jq -r 'select(.outcome=="HandoffAgent") | .prompt' prs.jsonl | dispatch_agents ;;
  3)     jq -r 'select(.outcome=="HandoffHuman")' prs.jsonl | notify_human ;;
  1|2|6) jq . prs.jsonl >&2; escalate ;;
  64)    echo "fix invocation" >&2 ;;
esac
```

## Suite grammar

```
<argv>           ::= <flag-or-inspect>* <group> ( ',' <group> )* <flag>*
<flag-or-inspect>::= <flag> | 'inspect'
<group>          ::= <flag>* <slug-or-pr> ( <flag>* <pr> )* <flag>*
<slug-or-pr>     ::= <slug> | <pr>            (must be <slug> if any '/' is in token)
<slug>           ::= /[^/[:space:]]+\/[^/[:space:]]+/
<pr>             ::= /[1-9][0-9]*/
<flag>           ::= '--max-iter' INT | '--concurrency' INT | '--state-root' PATH
                  | '--trace' PATH | '--status-comment' | '-h' | '--help'
```

The grammar reflects the parser's left-to-right scan: any token
that begins with `--` (or is `-h`) is consumed as a flag together
with its value if any; `inspect` is consumed as the mode subcommand
**only if** no positional has yet been pushed (otherwise it
attempts to parse as a PR token and fails); all other tokens
become positionals. After the scan, the positional vector is
joined with spaces and split on `,` to form groups; commas may
therefore appear as standalone tokens or as suffixes/prefixes on
adjacent tokens, all of which the split treats uniformly.

**Slug resolution:**

- A `<slug>` is detected by the presence of `'/'` in the **first
  token of a group only**. Once a slug is consumed, every remaining
  token in that group is parsed as `<pr>`; a `/` in a non-first
  token causes the PR parse to fail (`UsageError`).
- A group with **no explicit slug** inherits the prior group's
  slug. The very first group, if it has no explicit slug, falls
  back to inferring the cwd's repository via `gh repo view --json
nameWithOwner`.
- Cwd inference failure (no `gh`, not a github repo, malformed
  output) is a `UsageError` with the diagnostic.

**Distinct elements:** the parser rejects duplicate `(slug, pr)`
pairs as `UsageError("duplicate PR: <slug>#<pr>")` — duplicates are
**not** silently de-duplicated. Comma-separated groups must be
non-empty (lone `,,` rejects).

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

| Flag                | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--max-iter N`      | **Per-PR** iteration cap. Default 50. Must be ≥ 1. Inspect mode runs once per PR (cap unused, but `--max-iter 0` still rejects).                                                                                                                                                                                                                                                                                                                                                                                       |
| `--concurrency K`   | Maximum simultaneously-active PRs (workers). Default is the suite size (no cap). Must be ≥ 1; `K = 0` is rejected at the parser. `K > suite size` is silently clamped to the suite size. With `K < suite size`, `K` worker threads pull PRs from an atomic counter — a worker may handle multiple PRs sequentially while another is on its first PR.                                                                                                                                                                   |
| `--status-comment`  | Post status comments to each PR every iteration. Per-PR dedup at `<state-root>/github.com/<owner>/<repo>/prs/<pr>/status-comment/dedup.json`.                                                                                                                                                                                                                                                                                                                                                                          |
| `--state-root PATH` | Override the always-on state root. Default-resolution chain: `$OODA_PR_STATE_HOME` → `$XDG_STATE_HOME/ooda-pr` → `$HOME/.local/state/ooda-pr` → `$TMPDIR/ooda-pr` (i.e. `std::env::temp_dir().join("ooda-pr")`). Every step ends in `…/ooda-pr` — the slug is the **state-schema** name, deliberately shared with `/ooda-pr` so per-PR ledgers coexist across both skills. There is no `OODA_PRS_STATE_HOME`; the `OODA_PR_` prefix is the canonical env var.                                                          |
| `--trace PATH`      | Also append the compact trace to PATH. Each PR's `Recorder` opens its own file handle on PATH and appends; under `--concurrency > 1` writes from different PRs are not slug-prefixed and the per-line atomicity that stderr enjoys is **not** guaranteed across distinct file handles, so concurrent appends to one trace file may interleave mid-line on some filesystems. Prefer the per-PR `runs/<run-id>/trace.md` files (one file per PR per run, always written, no cross-PR interleaving) for live attribution. |
| `-h`, `--help`      | Print usage to stdout, exit 0. Pre-scan short-circuits all other validation including flag-repetition checks (`--help --help` exits 0).                                                                                                                                                                                                                                                                                                                                                                                |

**Repeating a flag** (`--max-iter`, `--concurrency`,
`--status-comment`, `--state-root`, `--trace`) is a `UsageError`,
**except** `-h` / `--help`, whose pre-scan short-circuits all
parser validation.

## Always-on state

Each invocation writes both per-PR and suite-level audit trails.

### Per-PR (shared with `/ooda-pr`)

Same layout as `/ooda-pr`. Keyed by forge + repo + PR, so the same
PR driven from `/ooda-pr` and `/ooda-prs` shares one host-local
memory.

```text
<root>/github.com/<owner>/<repo>/prs/<pr>/
  latest/        index.md, state.json, decision.json, outcome.json,
                 blockers.md, next.md
                 (action.json present when the latest decision carries
                  an Action; absent for halt-without-action cases —
                  Success / Terminal — which call remove_latest)
  ledger.md      cross-run; one bullet line per event:
                   `- <rfc3339-ts> <run-id> <summary>`
  ledger.jsonl   cross-run; one JSON object per event:
                   `{schema_version, timestamp, run_id, kind, summary}`
                 (jsonl carries `kind` and `schema_version`; md is a
                  visualization that drops them — not the same fields)
  events.jsonl   all events, all runs (single file appended-to)
  status-comment/dedup.json
  blobs/sha256/<aa>/<bb>/<hash>.zst                        # zstd-compressed,
                                                           # content-addressed
  runs/<run-id>/
    manifest.json                                          # one per run
    trace.md      human-readable run header + per-iter lines
    trace.jsonl   structured run-level events (per-run, distinct from
                  the cross-run events.jsonl above)
    iterations/<NNNN>/
      event-range.json
      state.json, decision.json, action.json (when present)
      tool-calls/<call-id>/                                # nested under
        stdout.bin, stderr.bin, record.json                # the iteration,
                                                           # not at run_root
```

### Suite-level (new in `/ooda-prs`)

```text
<root>/suites/<suite-id>/
  manifest.json    -- schema_version, suite_id, started_at, forge,
                      mode, max_iter, status_comment, concurrency,
                      cwd, argv, suite (Vec⟨{slug, pr}⟩)
  pointers.json    -- schema_version, suite_id,
                      prs : Vec⟨{slug, pr, run_id}⟩
  outcome.json     -- schema_version, suite_id, finished_at,
                      exit_code, multi_outcome
  trace.md         -- human-readable header (written at open) +
                      per-PR results table (appended at finalize) +
                      aggregate exit line
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
    DoneMerged
  ⊕ StuckRepeated(Action)
  ⊕ StuckCapReached(Action)
  ⊕ HandoffHuman(Action)
  ⊕ WouldAdvance(Action)
  ⊕ HandoffAgent(Action)
  ⊕ BinaryError(String)
  ⊕ Paused
  ⊕ DoneClosed
  ⊕ UsageError(String)                                 -- parser-only; never appears
                                                           in a ProcessOutcome
```

Each PR's `Outcome` is bit-equivalent to running `/ooda-pr` on that
PR alone. The suite boundary lifts these per-PR Outcomes to a
single binary boundary for shell dispatch.

`Outcome::UsageError` is listed in the per-PR `Outcome` type for
shape-completeness, but `/ooda-prs` constructs it **only** at the
suite parser and lifts it directly to `MultiOutcome::UsageError` —
it never appears inside a `ProcessOutcome`. Stdout JSONL records
therefore carry only the 9 reachable variants (see Output
channels).

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
WouldAdvance > non-actionable`. The "non-actionable" class is the
union `{DoneMerged, DoneClosed, Paused}`: every PR is either fully
merged (exit-0 per-PR), already closed (exit-8 per-PR), or has no
candidate action this pass (exit-7 per-PR — "Paused" is _not_ a
terminal lifecycle state, just a poll-back-later signal). All three
collapse to suite-level `$? = 0`; per-PR JSONL records disambiguate
which of the three each PR landed on.

This contract is **coarser** than `/ooda-pr`'s 1:1 variant→exit
mapping by design: a single byte of `$?` cannot encode N PRs
losslessly when `N > 1`, so the suite boundary projects per-PR
state into the action class the harness needs ("any agent work? any
errors?"). The fine-grained per-PR state lives on stdout (JSONL,
PR-keyed by construction).

## Output channels

Three channels, structurally distinct:

### Stdout — JSONL records (the agent-harness contract)

After all PRs halt, one record per PR is emitted to stdout in
**input order**. Each record is a single line of JSON:

**Loop-mode example** (no `inspect` subcommand; only halt-state
outcomes appear):

```jsonl
{"slug":"acme/widget","pr":1,"outcome":"DoneMerged","exit":0}
{"slug":"acme/widget","pr":2,"outcome":"HandoffAgent","exit":5,"action":"AddressThreads","blocker":"unresolved_threads","prompt":"Address 2 unresolved review threads.\n\n1. Copilot @ src/foo.rs:42\n   > <body>"}
{"slug":"acme/infra","pr":100,"outcome":"HandoffHuman","exit":3,"action":"RequestApproval","blocker":"not_approved","prompt":"Request or self-approve"}
```

**Inspect-mode example** (`ooda-prs inspect …`; advancing actions
become `WouldAdvance` because `act` is skipped):

```jsonl
{"slug":"acme/widget","pr":1,"outcome":"WouldAdvance","exit":4,"action":"MarkReady","blocker":"draft","automation":"Full"}
{"slug":"acme/infra","pr":100,"outcome":"WouldAdvance","exit":4,"action":"WaitForCi","blocker":"ci_pending: build","automation":"Wait(1m)"}
```

The two modes do not mix in a single invocation. Loop-mode bundles
never carry `WouldAdvance`. Inspect-mode bundles carry no
`StuckRepeated` (no second iteration to compare) and no
`StuckCapReached` (no cap is consulted). They CAN carry
`WouldAdvance` (the inspect-only artifact for an `Execute(action)`
decision), every `Handoff*` variant, every terminal/`Paused`
variant, and `BinaryError`.

Schema (the `automation` field is rendered by `format_automation`,
which delegates to `format_duration` for the `Wait{interval}` arm;
`format_duration` produces `<seconds>s`, `<minutes>m`, or
`<minutes>m<seconds>s`, picking whichever form is non-redundant for
the duration's value):

| Field        | Type    | Always present? | Notes                                                                                                                                                                                                                                             |
| ------------ | ------- | :-------------: | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `slug`       | string  |       yes       | `<owner>/<repo>`                                                                                                                                                                                                                                  |
| `pr`         | integer |       yes       | positive integer                                                                                                                                                                                                                                  |
| `outcome`    | string  |       yes       | variant name; **9 reachable values** in stdout: `DoneMerged`, `StuckRepeated`, `StuckCapReached`, `HandoffHuman`, `WouldAdvance`, `HandoffAgent`, `BinaryError`, `Paused`, `DoneClosed`. `UsageError` is suite-level only and never appears here. |
| `exit`       | integer |       yes       | per-PR exit code in `{0, 1, 2, 3, 4, 5, 6, 7, 8}` — the 1:1 mapping inherited from `/ooda-pr`. `64` does **not** appear in JSONL records (UsageError emits no stdout).                                                                            |
| `action`     | string  |   conditional   | present iff `outcome ∈ {StuckRepeated, StuckCapReached, HandoffHuman, HandoffAgent, WouldAdvance}` — the `ActionKind::name()` (e.g. `"Rebase"`, `"AddressThreads"`)                                                                               |
| `blocker`    | string  |   conditional   | same condition as `action` — the `BlockerKey` payload, a non-empty stable identifier. Typical values are ASCII with `:` and spaces (e.g. `"ci_fail: Build / test"`), but no surface form is contractual; consumers must not parse it.             |
| `prompt`     | string  |   conditional   | `outcome ∈ {HandoffAgent, HandoffHuman}` — verbatim agent/human prompt from `Action.description`. Multi-line content is JSON-string-escaped (literal `\n` in the JSON source, real newlines after `jq -r` decoding).                              |
| `automation` | string  |   conditional   | `outcome = WouldAdvance` only. Reachable values are `"Full"` and `"Wait(<duration>)"` — `Decision::Execute` is structurally restricted to `{Full, Wait{..}}`, so `"Agent"` and `"Human"` cannot appear.                                           |
| `msg`        | string  |   conditional   | `outcome = BinaryError` only — single-line human-triage string (newlines flattened to spaces by the binary)                                                                                                                                       |

`UsageError` (parse failure) emits **no stdout** — `$? = 64` and
the stderr usage block are sufficient. The JSONL stream is a clean
"every PR ran" contract.

### Stderr — per-iteration logs + per-PR variant blocks

**Loop mode.** Each PR's worker thread emits the same per-iteration
log lines as `/ooda-pr`:

```
[iter N] <ActionKind> (<Automation>) blocker: <BlockerKey>     -- Execute decisions
[iter N] halt: <DecisionHaltName>                              -- Success / Terminal halts
[iter N] halt: <DecisionHaltName> blocker: <BlockerKey>        -- AgentNeeded / HumanNeeded halts
                                                                  (the action's blocker is appended)
```

After the per-iteration lines, each PR emits its final variant
block (the `Outcome` rendered the same way `/ooda-pr` renders it on
exit — header line plus optional prompt block for `Handoff*`).

**Inspect mode** runs no iteration loop, so it emits no `[iter N]`
lines at all; only an optional one-shot `stack: <base> → <root>`
diagnostic and the variant block reach stderr.

**Concurrent interleaving.** Lines from different PRs are NOT
slug-prefixed. The OS preserves per-line atomicity for stderr
writes (one full `eprintln!` per write), so individual lines stay
intact, but with `--concurrency > 1` you cannot tell from stderr
alone which PR emitted which line. For attributable per-PR audit
use one of:

- The per-PR `runs/<run-id>/trace.md` (always written, **live**:
  appended as iterations happen — the only PR-attributed source of
  per-iteration logs during a run).
- The suite-level `<state-root>/suites/<suite-id>/trace.md` (a
  header written at open; the per-PR results table is appended
  only at finalize, after all workers join — useful for post-run
  audit, **not** for live attribution).
- The stdout JSONL stream (already PR-keyed by construction;
  available only after the binary exits).

The `--trace PATH` file inherits the same un-prefixed concurrency
hazard — prefer the per-PR or suite-level trace files when you need
attribution.

### `$?` — aggregate dispatch

The single-byte coarse dispatch (see table above).

## Harness pattern

```
# Loop mode — drive the suite to a halt, dispatch when needed,
# re-invoke until $? = 0.
loop:
  ~/.claude/skills/ooda-prs/run <suite> > prs.jsonl
  case $? in
    0)  break ;;                                          # all converged
    5)  jq -r 'select(.outcome=="HandoffAgent") | .prompt' prs.jsonl \
          | dispatch_subagents_parallel ;;                # then continue (re-invoke)
    3)  jq -r 'select(.outcome=="HandoffHuman")' prs.jsonl \
          | notify_human; break ;;                        # block on human
    1)  jq . prs.jsonl >&2; escalate_stuck ;;             # stall — diagnose
    2)  jq . prs.jsonl >&2; raise_max_iter_or_escalate ;; # cap reached
    6)  jq . prs.jsonl >&2; triage_binary_error ;;        # observe/act error
    64) fix_invocation; break ;;                          # parser error — fatal
  esac
```

For `inspect` mode the only additional class is `$? = 4`
(`WouldAdvance`); it never appears in loop mode. Inspect callers
add an arm:

```
4) jq -r 'select(.outcome=="WouldAdvance") | "\(.slug)#\(.pr): \(.action) (\(.automation))"' prs.jsonl
   ;;     # report what loop mode would do, then exit (inspect is one-shot)
```

The single-PR pattern from `/ooda-pr` (one Outcome → one dispatch
→ re-invoke) generalizes cleanly: the harness now dispatches **N
parallel sub-agents** when `$? = 5`, then re-invokes `/ooda-prs`.
This is the leverage of the multi-PR mode — `N` agents work in
parallel rather than serializing through one parent thread.

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

- The suite spawns `K = min(--concurrency, |suite|)` worker
  threads under `std::thread::scope`. Each worker pulls the next
  PR index from an `AtomicUsize` counter and runs `run_loop` for
  that PR; when finished, it pulls the next index. With
  `K = |suite|` (the default) every PR is on its own dedicated
  thread; with `K < |suite|` workers serially process multiple PRs.
- The binary exits only when all worker threads have joined and
  each `(slug, pr)` in the suite has been driven exactly once.
- A `HandoffAgent` for PR_i halts only PR_i's worker on that PR's
  iteration — the worker becomes free to pick up a next PR (under
  cap) and sibling PRs already in flight continue. This is the
  source of the multi-PR leverage.
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

[P3] ooda-prs terminates in bounded wall time: each per-PR run
     (run_loop or run_inspect) is bounded by --max-iter × max
     per-iteration cost, and `thread::scope` joins all workers
     before main returns. Total wall time ≤ ⌈|suite| / K⌉ ×
     max-per-PR-runtime when workers serialize PRs (the K=1 limit
     is plain serial; K=|suite| is full parallel, ≤ max-per-PR-
     runtime). No per-PR thread can prevent another from making
     progress.

[P4] MultiOutcome::exit_code is total. (See `multi_outcome.rs`.)

[P5] CLI parser is total over Argv: every input → either valid
     Suite or UsageError(_).

[P6] Recorder soundness: per-PR Recorder is single-writer per
     `(slug, pr)` (only one worker ever holds it). Suite Recorder
     manifest is written from the main thread at open; per-PR
     pointer registrations (`register_pr`) come from worker threads,
     serialized through `Arc<Mutex<_>>`; the final outcome.json and
     trace.md summary are written from the main thread after
     `thread::scope` joins. No write path admits concurrent writers
     to the same file.

[P7] The harness pattern composes: `$?` partitions every
     invocation into one of `{converged: 0}`, `{actionable agent
     work: 5}`, `{actionable human work: 3}`, `{actionable
     diagnostic: 1, 2, 6}`, `{inspect-only artifact: 4}`, or
     `{fatal parser error: 64}`. The per-PR JSONL records carry
     the fine-grained per-PR state for each branch.

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
