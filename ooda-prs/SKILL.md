---
name: ooda-prs
description: Drive N PRs through observe → orient → decide → act in parallel. Each invocation produces one MultiOutcome the caller dispatches on. Aggregate priority projection on `$?`; per-PR records on stdout (JSONL); per-PR variant blocks on stderr.
args:
  - name: <suite>
    description: One or more `<owner/repo>? <pr>+` groups, comma-separated. Required. (See "Suite grammar" in the body for the full BNF.)
  - name: inspect
    description: Optional subcommand. Must appear before the first positional token; flags may interleave freely. Runs one observe/orient/decide pass per PR; no act, no loop.
  - name: --max-iter N
    description: Per-PR loop iteration cap. Default 50; must be ≥1 (rejected with UsageError otherwise, even in inspect mode where the cap value is unused).
  - name: --concurrency K
    description: Maximum simultaneously-active PRs (workers). Parser default is None; None resolves to the suite size at the spawn loop (no cap). Must be ≥1; the unspecified-default case serializes as `null` in `manifest.json`.
  - name: --status-comment
    description: Post a status comment to each PR every iteration. Per-run deduped via the always-on state root.
  - name: --state-root PATH
    description: Override the always-on local state root for this invocation.
  - name: -h, --help
    description: Print usage to stdout and exit 0. Only invocation that writes structured stdout other than the JSONL stream.
---

# /ooda-prs

Drives **N PRs concurrently** through observe → orient → decide →
act until each halts. `/ooda-prs` is a fork of `/ooda-pr` and
duplicates the per-PR pipeline source code module-by-module; the
**recorder** module diverges (per-PR thread-local instead of
process-global) so concurrent worker threads do not alias their
tool-call sinks. The on-disk model is the shared `ooda-state`
crate (`<state-root>/runs/<run-id>/{events.jsonl, blobs/}` plus
`live/<run-id>` markers); decide/act semantics are unchanged from
`/ooda-pr`. Each invocation returns one `MultiOutcome`; the caller
dispatches on the aggregate exit code, parses the per-PR JSONL
records on stdout (each record carries an opaque `run_id` that
keys back to the per-run audit trail), and surfaces stderr to
humans for triage.

## Names

| Name        | Refers to                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/ooda-prs` | The skill (this document).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `ooda-prs`  | The compiled Rust binary at `target/release/ooda-prs`, sibling to this `SKILL.md` in the source tree.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `run`       | The wrapper script at `~/.claude/skills/ooda-prs/run`. Runs `(cd "$DIR" && cargo build --release --quiet) >&2` in a **subshell** so the parent shell's cwd is untouched — that's what preserves the user's cwd into the `exec`d binary (cwd-slug inference via `gh repo view` depends on it). Then `exec`s `"$DIR/target/release/ooda-prs" "$@"`. The wrapper uses `set -euo pipefail`, so a cargo build failure causes `run` to exit with cargo's non-zero exit code **before** the binary executes — that exit code is NOT one of the `Outcome` exit codes in the contract below; treat cargo build failures as a build-system error class distinct from `BinaryError`. |
| Suite       | The non-empty, distinct `Vec⟨(RepoSlug, PullRequestNumber)⟩` parsed from the suite grammar (duplicates are rejected as `UsageError`, not silently de-duplicated). Drives `run_loop` per pair in loop mode; drives `run_inspect` per pair in inspect mode.                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `/ooda-pr`  | The single-PR sibling skill, installed at `~/.claude/skills/ooda-pr/`. `/ooda-prs` does not depend on `/ooda-pr` being installed at runtime — the per-PR pipeline (observe / orient / decide / act / runner) is a code-level duplicate inside `/ooda-prs/src/`. The `recorder` module differs from `/ooda-pr`'s: `/ooda-prs` uses a `thread_local!` recorder cell so per-PR threads do not alias their tool-call sinks. Both binaries write the same on-disk `ooda-state` model (`<state-root>/runs/<run-id>/{events.jsonl, blobs/}` plus `live/<run-id>` markers), so the two skills' audit trails coexist on the same state root.                                       |

Always invoke `run`; never the binary directly.

## Type spine

Per-PR boundary types are defined in the `ooda-core` library
crate (`/home/cory/code/skills/ooda-core/`) and shared with the
three sibling OODA binaries. `ooda-prs` depends on `ooda-core`
via path dep and instantiates each generic type over its
domain-specific `ActionKind` enum (identical to `/ooda-pr`'s):

```rust
pub type Outcome      = ooda_core::Outcome<ActionKind>;
pub type Decision     = ooda_core::Decision<ActionKind>;
pub type DecisionHalt = ooda_core::DecisionHalt<ActionKind>;
pub type HaltReason   = ooda_core::HaltReason<ActionKind>;
pub type Action       = ooda_core::Action<ActionKind>;
```

`Automation`, `Urgency`, `TargetEffect`, `BlockerKey`, `Terminal`,
and the `ActionKindName` trait are re-exported from `ooda-core`.
The suite-level `MultiOutcome` type stays per-binary — it's
specific to `/ooda-prs`'s aggregate priority projection over N
PRs (see "MultiOutcome" below).

**Variant name ≠ stderr / JSONL header.** Rust variant names
(`DoneSucceeded`, `DoneAborted`, `Paused`) are internal. Stderr
headers and the JSONL `outcome` field both emit the PR-domain
strings (`DoneMerged`, `DoneClosed`, `Paused`) via the
per-binary `render_outcome` and `outcome_variant_name`
functions — so the documented regex
`^(DoneMerged|DoneClosed|Paused)$` and the JSONL contract are
unchanged. The Outcomes table below shows both representations.

**Per-binary code (not lifted):** `runner.rs` (per-PR
iteration loop), `recorder.rs` with `thread_local!` cell for
parallel workers, `multi_outcome.rs`, `decide/action.rs::ActionKind`
and its `ActionKindName` impl, and the per-binary
`From<LoopError> for Outcome`.

**Shared crates the recorder depends on:** `ooda-state` (the
domain-agnostic on-disk model — `runs/<run-id>/{events.jsonl, blobs/}`
plus `live/<run-id>` markers) and `ooda-core` (boundary types).
The recorder is a thin per-PR adapter onto `ooda-state::RunWriter`.

See `ooda-core/README.md` and `ooda-core/src/lib.rs` for the
shared-spine design rationale.

## Calling discipline

**`$?` MUST reflect ooda-prs's exit when ooda-prs runs.** The same
two concerns from `/ooda-pr` apply verbatim:

1. **ooda-prs must actually run.** `false && ooda-prs ...`
   short-circuits and ooda-prs never executes.
2. **Nothing may inject another exit code into `$?`.** Pipes
   (`ooda-prs | jq`) replace `$?` with the last pipeline element's
   exit code unless `set -o pipefail` is in effect (or you read
   `${PIPESTATUS[0]}` in bash / `${pipestatus[1]}` in zsh).
   Backgrounding (`ooda-prs &`) replaces `$?` with the background
   spawn's status. Any subsequent command (`ooda-prs; echo x`)
   replaces `$?` with the subsequent command's status.

   **Command substitution** (`out=$(ooda-prs …)`) **preserves**
   `$?` — the inner command's exit code is what `$?` reads
   immediately after the assignment. It is a safe capture
   pattern. (File redirection `> prs.jsonl` is also safe.)

**Capturing stdout** (the JSONL records) safely: either capture via
command substitution (`out=$(ooda-prs ...)`) or redirect to a file
(`ooda-prs ... > prs.jsonl`); both preserve `$?`. Pipes through `jq`
require either `set -o pipefail` first, or the
`${PIPESTATUS[0]}` (bash) / `${pipestatus[1]}` (zsh) idiom, or a
tempfile staging step.

```bash
# Capture stdout to a file (preserves $?), then dispatch on $?.
# Loop-mode invocation; no `2)` arm — exit 2 (WouldAdvance) is
# inspect-only and cannot occur in loop mode.
~/.claude/skills/ooda-prs/run acme/widget 1 2 > prs.jsonl
case $? in
  0)        echo "all done (all PRs terminal/Paused)" ;;
  1)        echo "all PRs Paused (re-invoke later)" ;;
  3)        jq -r 'select(.outcome=="HandoffHuman")' prs.jsonl | notify_human ;;
  4)        jq -r 'select(.outcome=="HandoffAgent") | .prompt' prs.jsonl | dispatch_agents ;;
  6|7|70)   jq . prs.jsonl >&2; escalate ;;
  64)       echo "fix invocation" >&2 ;;
  130|143)  echo "signal-killed ($? — SIGINT/SIGTERM)" >&2 ;;
  *)        echo "unknown exit $? — likely a cargo build failure (see 'run' wrapper)"; \
            exit 1 ;;
esac
```

The `*)` default arm catches non-`Outcome` exit codes the `run`
wrapper itself can produce. Cargo build failures inside the
wrapper's subshell propagate through `set -euo pipefail` with
cargo's own exit code — commonly `101` for compile errors and
`1` for many cargo-cli failures.

**The redesigned exit-code scheme separates information-bearing
halts (1–7) from system errors (64, 70).** Cargo's `1` no longer
aliases any `Outcome` variant (`StuckRepeated` is now `6`), and
the `*)` default arm cleanly catches cargo-1, `101`, and any
`128 + signal` code the dispatch table doesn't enumerate.
Wrapper failures fall through to `*)` predictably.

**Surface per-PR handoffs to the user verbatim.** When a downstream
consumer (`notify_human`, `dispatch_agents`, an interactive
wrapper agent) presents a per-PR `HandoffHuman` / `HandoffAgent`
record to a human, the surface MUST be verbatim: the JSONL
record's `prompt` field already carries the dashboard preamble +
per-action body that explain why this PR halted. Do not collapse
into a one-line summary; the human needs the full body to
(a) understand each halt without opening per-PR state files,
(b) verify the orchestrator's interpretation before approving the
next action, and (c) catch cases where the orchestrator is about
to act on a wrong reading. Format is the consumer's choice
(verbatim fenced block, structured render, collapsible per-PR
section, etc.) — fidelity is the constraint, not format. The
per-PR handoff blob at
`<state-root>/runs/<run-id>/blobs/<sha>.md` (pointed at by the
stderr `see:` line) carries the same content for any consumer
that prefers files over JSONL field reads. See
`/ooda-pr` SKILL.md §`Handoff*` prompt format → "Surface to
the user" for the single-PR rationale; the same applies per-PR
in the suite.

## Suite grammar

The grammar is **token-scan-based**: any argv token that begins
with `--` (or is exactly `-h`) is consumed as a flag at the
position where it appears; an unrecognized `--<name>` is rejected
as `UsageError`; any other token accumulates into a positional
vector that the parser then splits on `,`. (Single-dash tokens
other than `-h` — e.g. `-x`, `-1` — are NOT recognized as flags;
they fall through to positional and fail later as malformed PR
numbers.) Recognized flags may interleave anywhere — before the
first group, between groups, between PR tokens within a group, or
trailing — without changing the parsed suite.

The compact production below uses set notation rather than
sequence notation (Kleene-star over alternation) to capture this:

```
<argv>      ::= ⟨ tokens ⟩ where:
                    flag-tokens ⊆ <flag>+ (any positions, free interleave)
                    inspect-token ∈ {'inspect'}? (at most one; if present,
                                    must appear before any positional token)
                    positional-tokens, when split on ',' and re-tokenized
                                       on whitespace, yield ≥ 1 <group>
<group>     ::= <slug-or-pr> <pr>*
<slug-or-pr>::= <slug> | <pr>
                  -- the FIRST token of a group containing '/' MUST satisfy
                     <slug>'s shape (`RepoSlug::parse` rejects 0 or ≥2
                     slashes); a non-first token containing '/' attempts
                     <pr>::parse and fails. Tokens with no '/' parse as
                     <pr>.
<slug>      ::= owner '/' repo
                  -- both `owner` and `repo` are non-empty and contain no
                     '/' (validated by `Owner::parse` / `Repo::parse`).
                     Whitespace exclusion comes from the upstream
                     whitespace tokenization, not the slug parser itself.
<pr>        ::= [0-9]+                      -- parsed as u64, then
                                               rejected if value = 0
                                               (PullRequestNumber::new
                                               requires > 0). Leading
                                               zeros are accepted at
                                               the parser ("07" → 7).
<flag>      ::= '--max-iter' POS_INT | '--concurrency' POS_INT
              | '--state-root' PATH | '--trace' PATH
              | '--status-comment' | '-h' | '--help'
POS_INT     ::= [0-9]+                      -- parsed as u32 via
                                               `v.parse::<u32>()`, then
                                               rejected if value = 0
                                               (must be ≥ 1). A leading
                                               `-` sign is detected
                                               (via `starts_with('-')`)
                                               and rejected with a
                                               distinct "got negative
                                               value" diagnostic. Leading
                                               zeros are accepted at
                                               the parser ("07" → 7),
                                               same as `<pr>`.
PATH        ::= any string                  -- path on the host filesystem
```

`--max-iter=10` and `--concurrency=2` are NOT accepted; the parser
expects the flag and value as separate argv tokens (via
`iter.next()`). Each flag (other than `-h` / `--help`) may appear
at most once; a repeat is a `UsageError`. `-h` / `--help` is
consumed by a pre-scan that runs **before** the main parse loop:
if either token appears anywhere in argv, usage is printed to
stdout and the process exits 0 — no other validation runs, so
`--help --help`, `--max-iter 0 --help`, etc. all exit 0. This
makes the grammar's "at most once" rule structurally inapplicable
to `-h` / `--help`. The `inspect` keyword may appear at most
once and only before any positional token; a second `inspect`
falls through to the positional vector and fails as a non-numeric
`<pr>`.

The parser's left-to-right scan: any token that begins with `--`
(or is `-h`) is consumed as a flag together with its value if
any; `inspect` is consumed as the mode subcommand **only if** no
positional has yet been pushed (otherwise it attempts to parse
as a PR token and fails); all other tokens become positionals.
After the scan, the positional vector is joined with spaces and
split on `,` to form groups; commas may
therefore appear as standalone tokens or as suffixes/prefixes on
adjacent tokens, all of which the split treats uniformly.

**Slug resolution:**

- A `<slug>` is detected by the presence of `'/'` in the **first
  token of a group only**. Once a slug is consumed, every remaining
  token in that group is parsed as `<pr>`; a `/` in a non-first
  token causes the PR parse to fail (`UsageError`).
- A group with **no explicit slug** inherits the prior group's
  slug. The very first group, if it has no explicit slug, falls
  back to inferring the cwd's repository via
  `gh repo view --json nameWithOwner --jq .nameWithOwner`.
- Cwd inference failure (no `gh`, not a github repo, malformed
  output) is a `UsageError` with the diagnostic.

**Distinct elements:** the parser rejects duplicate `(slug, pr)`
pairs as `UsageError("duplicate PR: <slug>#<pr>")` — duplicates are
**not** silently de-duplicated. Comma-separated groups must be
non-empty (lone `,,` rejects).

The table below shows just the **suite** portion of the argv for
brevity. Always invoke the wrapper at `~/.claude/skills/ooda-prs/run`
in real use (see "Calling discipline" above).

| Suite portion of argv               | Parsed suite                                                |
| ----------------------------------- | ----------------------------------------------------------- |
| `42 45`                             | `[(cwd, 42), (cwd, 45)]`                                    |
| `765 777 983`                       | `[(cwd, 765), (cwd, 777), (cwd, 983)]`                      |
| `acme/widget 42, acme/infra 100`    | `[(acme/widget, 42), (acme/infra, 100)]`                    |
| `acme/widget 42 43, acme/infra 100` | `[(acme/widget, 42), (acme/widget, 43), (acme/infra, 100)]` |
| `acme/widget 42, 43`                | `[(acme/widget, 42), (acme/widget, 43)]` (slug inheritance) |
| `42, acme/infra 100`                | `[(cwd, 42), (acme/infra, 100)]`                            |

## How to call

```bash
~/.claude/skills/ooda-prs/run [options] <suite>            # loop mode
~/.claude/skills/ooda-prs/run inspect [options] <suite>    # one pass per PR
```

| Flag                | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--max-iter N`      | **Per-PR** iteration cap. Default 50. Must be ≥ 1. Inspect mode runs once per PR (cap unused, but `--max-iter 0` still rejects).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `--concurrency K`   | Maximum simultaneously-active PRs (workers). Default is the suite size (no cap). Must be ≥ 1; `K = 0` is rejected at the parser. `K > suite size` is silently clamped to the suite size. With `K < suite size`, `K` worker threads pull PRs from an atomic counter — a worker may handle multiple PRs sequentially while another is on its first PR.                                                                                                                                                                                                                                                                                                   |
| `--status-comment`  | Post status comments to each PR every iteration. Per-run dedup at `<state-root>/runs/<run-id>/status_comment_dedup.json`. Dedup is scoped to a single run — re-invoking the binary opens a fresh run with no prior dedup memory.                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `--state-root PATH` | Override the always-on state root. Resolution chain (first that yields a value wins): (1) `--state-root PATH` if given, (2) `$OODA_STATE_HOME` if set and **non-empty**, (3) `$XDG_STATE_HOME/ooda` if `$XDG_STATE_HOME` is set and non-empty, (4) `$HOME/.local/state/ooda` if `$HOME` is set and non-empty, (5) `std::env::temp_dir().join("ooda")` (e.g. `/tmp/ooda` on Linux). Empty env vars are treated as **unset** (a `=""` value falls through). The state root is **domain-agnostic** — one root per machine, shared by every OODA agent. PR identity lives only inside event records (`target.{forge,slug,pr}`), never in the on-disk path. |
| `-h`, `--help`      | Print usage to stdout, exit 0. Pre-scan short-circuits all other validation including flag-repetition checks (`--help --help` exits 0).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |

**Repeating a flag** (`--max-iter`, `--concurrency`,
`--status-comment`, `--state-root`) is a `UsageError`, **except**
`-h` / `--help`, whose pre-scan short-circuits all parser
validation.

## Always-on state

An invocation that **parses successfully** opens one per-PR run
through [`ooda-state`]. On `UsageError` (parser failure) no run is
opened. A per-PR recorder-open failure turns into that PR's
`Outcome::BinaryError`; other PRs proceed independently.

### Layout

The on-disk model is **domain-agnostic**: paths carry no PR
identity. PR slug / PR number / mode / etc. live only inside event
records.

```text
<state-root>/
├── runs/<run-id>/
│   ├── events.jsonl                 # source of truth (append-only)
│   ├── blobs/<sha>.<ext>            # content-addressed payloads
│   └── status_comment_dedup.json    # per-run mutable; present only
│                                    # when --status-comment fires
└── live/<run-id>                    # empty marker; presence = "active"
```

One `runs/<run-id>/` directory per worker. Two PRs in the same
suite yield two distinct `<run-id>` directories under the shared
`<state-root>/runs/`; the `<run-id>` is opaque (`<YYYYMMDDTHHMMSSZ>-<nanos>-p<pid>`
shape, see `ooda_state::RunId::generate`).

### Event stream

`runs/<run-id>/events.jsonl` is the append-only source of truth.
Each line is a typed event with a `kind` discriminator:

| `kind`               | Carries                                                               |
| -------------------- | --------------------------------------------------------------------- |
| `run_started`        | `domain: "pr"`, `target: {forge, slug, pr, mode, max_iter, …}`        |
| `iteration_observed` | `iteration`, `blob` (normalized observation snapshot)                 |
| `iteration_oriented` | `iteration`, `blob` (oriented snapshot)                               |
| `iteration_decided`  | `iteration`, `decision_kind`                                          |
| `iteration_executed` | `iteration`, `action_kind`                                            |
| `iteration_waited`   | `iteration`, `action_kind`, `interval_ms`                             |
| `iteration_handoff`  | `iteration`, `variant`, `action_kind`, `blob` (prompt body)           |
| `run_halted`         | `outcome` (variant name), `exit_code`                                 |
| `domain_specific`    | `kind_suffix`, `payload` — observe / action / wait / tool-call frames |

The `domain_specific` kind is the catch-all the recorder uses for
PR-domain observability events that don't have a typed counterpart
in `ooda-state`. The `kind_suffix` distinguishes them at read time
(`observe_started`, `observe_finished`, `action_started`,
`action_finished`, `wait_started`, `wait_finished`,
`tool_call_started`, `tool_call_finished`,
`status_comment_rendered`, `status_comment_result`, `decision_envelope`,
`dashboard`, `outcome`, `trace_line`).

### Blobs

Iteration snapshots (normalized observation, oriented state),
handoff prompt bodies, and tool-call stdout / stderr captures are
written as content-addressed blobs in `runs/<run-id>/blobs/`. Each
event referencing a blob carries a `BlobRef` (`sha`, `size`,
`ext`). Dedup is per-run automatic — repeated identical bytes
write only one file.

The handoff prompt path surfaced on the stderr `see:` pointer
targets a blob directly:
`<state-root>/runs/<run-id>/blobs/<sha>.md`.

### Live marker

`<state-root>/live/<run-id>` is an empty file present from
`run_started` to terminal (`run_halted` / `run_stalled` /
`run_cap_reached`). Audit / cockpit tooling can enumerate
`live/` to find currently-active runs; on terminal events the
marker is removed.

### Concurrency

Each worker writes a distinct `runs/<run-id>/` directory; no two
workers share a subtree. The state root's `runs/` and `live/`
parents are shared but every leaf write is scoped to its own
run-id, so there is no cross-worker contention or torn-write
hazard at the recorder layer. Two simultaneous invocations
across overlapping PRs each get distinct `<run-id>`s; the
per-run dirs are disjoint by construction.

### Joining JSONL records back to the audit trail

Each per-PR stdout JSONL record (see "Output channels") carries a
`run_id` field. The corresponding on-disk audit trail lives at
`<state-root>/runs/<run-id>/events.jsonl`. There is no separate
suite-level manifest or pointer file — the `(slug, pr, run_id)`
triple on each stdout record is the index.

## MultiOutcome

```
MultiOutcome =
    UsageError(String)                                 -- parser failure; no PRs ran
  ⊕ Bundle(Vec⟨ProcessOutcome⟩)                        -- every PR reached a halt state

ProcessOutcome = (RepoSlug, PullRequestNumber, Outcome)

Outcome =                                              -- per-PR; identical to /ooda-pr
    DoneSucceeded                                      -- stderr: "DoneMerged"
  ⊕ StuckRepeated(Action)
  ⊕ StuckCapReached(Action)
  ⊕ HandoffHuman(Action)
  ⊕ WouldAdvance(Action)
  ⊕ HandoffAgent(Action)
  ⊕ BinaryError(String)
  ⊕ Paused
  ⊕ DoneAborted                                        -- stderr: "DoneClosed"
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

| Condition                                              | `$?` | Mode reachability   |
| :----------------------------------------------------- | :--: | :------------------ |
| `MultiOutcome::UsageError`                             |  64  | both (parser-level) |
| any `ProcessOutcome` carries `BinaryError(_)`          |  70  | both                |
| else any carries `HandoffAgent(_)`                     |  4   | both                |
| else any carries `HandoffHuman(_)`                     |  3   | both                |
| else any carries `StuckCapReached(_)`                  |  7   | loop only           |
| else any carries `StuckRepeated(_)`                    |  6   | loop only           |
| else any carries `WouldAdvance(_)`                     |  2   | inspect only        |
| else every PR ∈ `{DoneSucceeded, DoneAborted, Paused}` |  0   | both                |

**Priority order** (highest first): `UsageError > BinaryError >
HandoffAgent > HandoffHuman > StuckCapReached > StuckRepeated >
WouldAdvance > non-actionable`. Priority is **semantic**, not
numeric — the first matching condition wins, even though
`StuckRepeated`'s code (`6`) is numerically smaller than
`HandoffAgent`'s (`4`). The "non-actionable" class is the
union `{DoneSucceeded, DoneAborted, Paused}`: every PR is either fully
merged (exit-0 per-PR), already closed (exit-5 per-PR), or has no
candidate action this pass (exit-1 per-PR — "Paused" is _not_ a
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

Records below are **schematic in two ways**: (a) `prompt` strings
are abbreviated, and (b) field order is shown in a reader-friendly
order (`slug, pr, outcome, exit, …`) to match the schema table.
The live binary serializes via `serde_json` without the
`preserve_order` feature, so the actual on-disk order is
**alphabetical by key**. Parse the records as JSON; do not rely on
field position. Live alphabetical-order forms of the records below
would be: keys sorted lexicographically (e.g. `action`, `blocker`,
`exit`, `outcome`, `pr`, `prompt`, `slug` for HandoffAgent
records).

```jsonl
{"slug":"acme/widget","pr":1,"pr_url":"https://github.com/acme/widget/pull/1","run_id":"20260517T142500Z-000000123-p4242","outcome":"DoneMerged","exit":0}
{"slug":"acme/widget","pr":2,"pr_url":"https://github.com/acme/widget/pull/2","run_id":"20260517T142500Z-000000456-p4242","outcome":"HandoffAgent","exit":4,"action":"AddressThreads","blocker":"unresolved_threads","prompt":"Address 2 unresolved review threads.\nCopilot: 2 issues.\n\n1. Copilot @ src/foo.rs:42\n   > body line 1\n\n…"}
{"slug":"acme/infra","pr":100,"pr_url":"https://github.com/acme/infra/pull/100","run_id":"20260517T142500Z-000000789-p4242","outcome":"HandoffHuman","exit":3,"action":"RequestApproval","blocker":"not_approved","prompt":"Request or self-approve"}
```

**Inspect-mode example** (`ooda-prs inspect …`; advancing actions
become `WouldAdvance` because `act` is skipped):

```jsonl
{"slug":"acme/widget","pr":1,"pr_url":"https://github.com/acme/widget/pull/1","run_id":"20260517T142500Z-000000123-p4242","outcome":"WouldAdvance","exit":2,"action":"MarkReady","blocker":"draft","effect":"Full"}
{"slug":"acme/infra","pr":100,"pr_url":"https://github.com/acme/infra/pull/100","run_id":"20260517T142500Z-000000789-p4242","outcome":"WouldAdvance","exit":2,"action":"WaitForCi","blocker":"ci_pending: build","effect":"Wait(1m)"}
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
| `pr_url`     | string  |       yes       | `https://github.com/<owner>/<repo>/pull/<pr>`                                                                                                                                                                                                     |
| `run_id`     | string  |       yes       | Opaque [`ooda_state`] run id. Joins the record back to `<state-root>/runs/<run-id>/events.jsonl`. Empty string when the per-PR recorder failed to open (the same condition that produced `Outcome::BinaryError` for the PR).                      |
| `outcome`    | string  |       yes       | variant name; **9 reachable values** in stdout: `DoneMerged`, `StuckRepeated`, `StuckCapReached`, `HandoffHuman`, `WouldAdvance`, `HandoffAgent`, `BinaryError`, `Paused`, `DoneClosed`. `UsageError` is suite-level only and never appears here. |
| `exit`       | integer |       yes       | per-PR exit code in `{0, 1, 2, 3, 4, 5, 6, 7, 70}` — the 1:1 mapping inherited from `/ooda-pr`. `64` does **not** appear in JSONL records (UsageError emits no stdout); `130`/`143` are signal-synthesized and the binary never returns them.     |
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
[iter N] halt: <DecisionHaltName>                              -- Success / Terminal halts (no action)
[iter N] halt: <DecisionHaltName> blocker: <BlockerKey>        -- AgentNeeded / HumanNeeded halts
                                                                  (the action's blocker is appended)
```

`<DecisionHaltName>` is one of the literal strings `Success`,
`Terminal(Succeeded)`, `Terminal(Aborted)`, `AgentNeeded`,
`HumanNeeded` (from `DecisionHalt::name()`). The first three carry
no action; `AgentNeeded` / `HumanNeeded` carry an `Action` whose
blocker is appended in the second form.

When `--status-comment` is set in **loop mode**, each iteration may
also emit a comment-post log line: `[iter N] comment: posted` on a
successful post, or `[iter N] comment: <PostError>` on failure.
The dedup-skip case (no content change since last post) is **silent
in loop mode** — no `[iter N] comment: skipped (unchanged)` line is
emitted. (Inspect mode does emit `comment: skipped (unchanged)`
because the post step there sets `verbose_skip = true`.)

After the per-iteration lines, each PR emits its final variant
block (the `Outcome` rendered the same way `/ooda-pr` renders it on
exit — header line plus, for `Handoff*`, a single
`  see: <abs-path>` pointer line. The path points at a
content-addressed blob `<state-root>/runs/<run-id>/blobs/<sha>.md`
in the per-run blob store; the same `run_id` appears in the PR's
stdout JSONL record. The prompt body lives in the file, not on
stderr — read the file in full rather than tail-truncating stderr).

**Inspect mode** runs no iteration loop, so it emits no `[iter N]`
lines at all. What can reach stderr in inspect mode:

- An optional one-shot `stack: <base> → <root>` diagnostic when
  the PR's immediate base branch differs from the resolved stack
  root (suppressed when they match).
- When `--status-comment` is set: a `comment: posted`,
  `comment: skipped (unchanged)`, or `comment: <PostError>` line
  emitted by the post step.
- The final variant block (header + optional `  see: <path>`
  pointer line for `Handoff*` — prompt body lives in the
  per-run blob `<state-root>/runs/<run-id>/blobs/<sha>.md`,
  not on stderr).

**Concurrent interleaving.** Lines from different PRs are NOT
slug-prefixed. Rust's stderr lock serializes individual `eprintln!`
calls within a single process (the runtime acquires it per `print*!`
invocation), so short lines from concurrent workers do not splice
mid-character — but `eprintln!` may still issue multiple syscalls
for very long lines, and the lock is per-call, not per-line, so
fine-grained interleave between adjacent lines is possible. With
`--concurrency > 1` you in any case cannot tell from stderr alone
which PR emitted which line. For attributable per-PR audit use one
of:

- The per-PR `runs/<run-id>/events.jsonl` (always written, **live**:
  appended as iterations happen — the only PR-attributed source of
  per-iteration events during a run).
- The stdout JSONL stream (already PR-keyed by construction;
  carries `run_id` so each record joins back to its run dir;
  available only after the binary exits).

### `$?` — aggregate dispatch

The single-byte coarse dispatch (see table above).

## Harness pattern

The names below (`dispatch_subagents_parallel`, `notify_human`,
`escalate_stuck`, `raise_max_iter_or_escalate`,
`triage_binary_error`, `fix_invocation`) are **caller-supplied
placeholders** — `/ooda-prs` does not provide them. Substitute
your harness's own actions.

```
# Loop mode — drive the suite to a halt, dispatch when needed,
# re-invoke until $? = 0. The `[[ -x … ]]` precheck guards
# against wrapper / cargo build failures, whose exit codes fall
# through to the `*)` arm but might confuse logs.
loop:
  [[ -x ~/.claude/skills/ooda-prs/target/release/ooda-prs ]] || \
    { wrapper_build_failed; break; }
  ~/.claude/skills/ooda-prs/run <suite> > prs.jsonl
  case $? in
    0)       break ;;                                          # all converged
    1)       break ;;                                          # all Paused; re-invoke later
    3)       jq -r 'select(.outcome=="HandoffHuman")' prs.jsonl \
               | notify_human; break ;;                        # block on human
    4)       jq -r 'select(.outcome=="HandoffAgent") | .prompt' prs.jsonl \
               | dispatch_subagents_parallel ;;                # then continue (re-invoke)
    6)       jq . prs.jsonl >&2; escalate_stuck ;;             # stall — diagnose
    7)       jq . prs.jsonl >&2; raise_max_iter_or_escalate ;; # cap reached
    64)      fix_invocation; break ;;                          # parser error — fatal
    70)      jq . prs.jsonl >&2; triage_binary_error ;;        # observe/act error (sysexits EX_SOFTWARE)
    130|143) signal_killed; break ;;                           # SIGINT / SIGTERM
    *)       unknown_exit "$?"; break ;;                       # 101 / wrapper / etc.
  esac
```

For `inspect` mode the only additional class is `$? = 2`
(`WouldAdvance`); it never appears in loop mode. Inspect callers
add an arm:

```
2) jq -r 'select(.outcome=="WouldAdvance") | "\(.slug)#\(.pr): \(.action) (\(.automation))"' prs.jsonl
   ;;     # report what loop mode would do, then exit (inspect is one-shot)
```

The single-PR pattern from `/ooda-pr` (one Outcome → one dispatch
→ re-invoke) generalizes cleanly: when `$? = 4`, the harness
dispatches one sub-agent per `HandoffAgent` record on stdout —
that count is at most `|suite|` but is typically smaller, since
non-handoff PRs (already merged, blocked on a human, stuck, etc.)
contribute no records to dispatch. After the dispatched sub-agents
finish, the harness re-invokes `/ooda-prs`. This is the leverage
of the multi-PR mode: handoff dispatches happen in parallel
across PRs rather than serializing through one parent thread.

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
  PR index from an `AtomicUsize` counter and runs `run_loop` (loop
  mode) or `run_inspect` (inspect mode) for that PR; when finished,
  it pulls the next index. With `K = |suite|` (the default) every
  PR is on its own thread; with `K < |suite|` workers serially
  process multiple PRs.
- The binary exits when all worker threads have joined under
  `thread::scope` and each `(slug, pr)` in the suite has been
  driven exactly once. A `panic!` in any worker propagates at
  scope-exit and aborts the binary; partial per-PR ledgers from
  workers that ran to completion remain on disk.
- Each per-PR halt outcome (any of `DoneSucceeded`, `DoneAborted`,
  `Paused`, `StuckRepeated`, `StuckCapReached`, `HandoffHuman`,
  `WouldAdvance`, `HandoffAgent`, `BinaryError`) ends that PR's
  iteration. The worker that ran that PR returns to the atomic
  counter and pulls the next pending PR (under cap), and any
  sibling PRs already in flight continue independently. This is
  the source of the multi-PR leverage: a `HandoffAgent` for one
  PR does not pause the others.
- The aggregate Outcome reflects every PR's final state;
  re-invocation drives the next round.

## Per-PR ↔ suite invariants

Internal invariants of the design:

```
[P1] ∀ PR (slug, pr) ∈ suite, the trajectory of run_loop(slug, pr, …)
     inside ooda-prs is observationally indistinguishable from
     ooda-pr on the same (slug, pr) **on the per-PR audit channels**
     (per-PR Recorder events in `runs/<run-id>/events.jsonl`, the
     PR's own JSONL record on stdout). The shared stderr stream is
     NOT a per-PR audit channel under `--concurrency > 1`: lines
     from sibling PRs interleave there without slug prefixes.
     Per-PR semantic preservation holds at the recorder boundary.

[P2] ∀ distinct PR_i, PR_j ∈ suite, the action stream of PR_i is
     causally independent of PR_j's. (No shared mutable state.)

[P3] ooda-prs terminates in bounded wall time: each per-PR run
     (run_loop or run_inspect) is bounded by --max-iter × max
     per-iteration cost, and `thread::scope` joins all workers
     before main returns. With **rolling concurrency** (atomic-
     counter work index — see `suite::drive_suite`), workers pull
     PRs as soon as they finish, so a fast PR frees its worker for
     the next pending PR rather than waiting for a batch to
     complete. The wall-time bound therefore depends on the
     specific runtime distribution: at K = |suite| every PR runs
     concurrently and total wall time ≤ max-per-PR-runtime; at
     K = 1 plain serial, total wall time ≤ Σ per-PR-runtimes; for
     intermediate K and homogeneous runtimes, ≤ ⌈|suite|/K⌉ × max-
     per-PR-runtime; for heterogeneous runtimes the rolling
     scheduler beats this batched bound. No per-PR thread can
     prevent another from making progress.

[P4] MultiOutcome::exit_code is total. (See `multi_outcome.rs`.)

[P5] CLI parser is total over Argv: every input → either valid
     Suite or UsageError(_).

[P6] Recorder soundness: per-PR Recorder is single-writer per
     `(slug, pr)` (only one worker ever holds it) and writes to a
     dedicated `runs/<run-id>/` directory disjoint from every
     other worker's. The shared `<state-root>/runs/` and
     `<state-root>/live/` parents are touched only via per-run
     leaf paths, so no write path admits concurrent writers to the
     same file.

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
