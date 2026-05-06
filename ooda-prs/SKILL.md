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
act until each halts. `/ooda-prs` is a fork of `/ooda-pr` and
duplicates the per-PR pipeline source code module-by-module; the
**recorder** module diverges (per-PR thread-local instead of
process-global) so concurrent worker threads do not alias their
tool-call sinks. The on-disk schema and decide/act semantics are
unchanged. Each invocation returns one `MultiOutcome`; the caller
dispatches on the aggregate exit code, parses the per-PR JSONL
records on stdout, and surfaces stderr to humans for triage.

## Names

| Name        | Refers to                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/ooda-prs` | The skill (this document).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `ooda-prs`  | The compiled Rust binary at `target/release/ooda-prs`, sibling to this `SKILL.md` in the source tree.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `run`       | The wrapper script at `~/.claude/skills/ooda-prs/run`. Runs `(cd "$DIR" && cargo build --release --quiet) >&2` in a **subshell** so the parent shell's cwd is untouched — that's what preserves the user's cwd into the `exec`d binary (cwd-slug inference via `gh repo view` depends on it). Then `exec`s `"$DIR/target/release/ooda-prs" "$@"`. The wrapper uses `set -euo pipefail`, so a cargo build failure causes `run` to exit with cargo's non-zero exit code **before** the binary executes — that exit code is NOT one of the `Outcome` exit codes in the contract below; treat cargo build failures as a build-system error class distinct from `BinaryError`. |
| Suite       | The non-empty, distinct `Vec⟨(RepoSlug, PullRequestNumber)⟩` parsed from the suite grammar (duplicates are rejected as `UsageError`, not silently de-duplicated). Drives `run_loop` per pair in loop mode; drives `run_inspect` per pair in inspect mode.                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `/ooda-pr`  | The single-PR sibling skill, installed at `~/.claude/skills/ooda-pr/`. The fork shares the on-disk **state schema** (state-root chain, per-PR ledger layout) but does not depend on `/ooda-pr` being installed at runtime — the per-PR pipeline (observe / orient / decide / act / runner) is a code-level duplicate inside `/ooda-prs/src/`. The `recorder` module differs between the two: `/ooda-prs` uses a `thread_local!` recorder cell so per-PR threads do not alias their tool-call sinks. The on-disk format is unchanged.                                                                                                                                      |

Always invoke `run`; never the binary directly.

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
# Loop-mode invocation; no `4)` arm — exit 4 (WouldAdvance) is
# inspect-only and cannot occur in loop mode.
~/.claude/skills/ooda-prs/run acme/widget 1 2 > prs.jsonl
case $? in
  0)     echo "all done" ;;
  5)     jq -r 'select(.outcome=="HandoffAgent") | .prompt' prs.jsonl | dispatch_agents ;;
  3)     jq -r 'select(.outcome=="HandoffHuman")' prs.jsonl | notify_human ;;
  1|2|6) jq . prs.jsonl >&2; escalate ;;
  64)    echo "fix invocation" >&2 ;;
  *)     echo "unknown exit $? — likely a cargo build failure (see 'run' wrapper)"; \
         exit 1 ;;
esac
```

The `*)` default arm catches non-Outcome exit codes the `run`
wrapper itself can produce. Cargo build failures inside the
wrapper's subshell propagate through `set -euo pipefail` with
cargo's own exit code — commonly `101` for compile errors (caught
by `*)`) and `1` for many cargo-cli failures.

**Cargo's exit `1` numerically aliases `Outcome::StuckRepeated`'s
exit code 1**, and shell `case` matches arms top-to-bottom — so
the `1|2|6)` arm above matches a cargo-1 failure first; the `*)`
arm cannot guard against that specific collision. The only
reliable guard is to verify the binary exists before invoking the
wrapper, e.g. `[[ -x ~/.claude/skills/ooda-prs/target/release/ooda-prs ]]`,
or check the wrapper's exit code separately from the dispatch
table. The `*)` arm catches `101` and other non-Outcome codes
(`128 + signal`, etc.) but not the `1` alias.

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

| Flag                | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--max-iter N`      | **Per-PR** iteration cap. Default 50. Must be ≥ 1. Inspect mode runs once per PR (cap unused, but `--max-iter 0` still rejects).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `--concurrency K`   | Maximum simultaneously-active PRs (workers). Default is the suite size (no cap). Must be ≥ 1; `K = 0` is rejected at the parser. `K > suite size` is silently clamped to the suite size. With `K < suite size`, `K` worker threads pull PRs from an atomic counter — a worker may handle multiple PRs sequentially while another is on its first PR.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `--status-comment`  | Post status comments to each PR every iteration. Per-PR dedup at `<state-root>/github.com/<owner>/<repo>/prs/<pr>/status-comment/dedup.json`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `--state-root PATH` | Override the always-on state root. Resolution chain (first that yields a value wins): (1) `--state-root PATH` if given, (2) `$OODA_PR_STATE_HOME` if set and **non-empty**, (3) `$XDG_STATE_HOME/ooda-pr` if `$XDG_STATE_HOME` is set and non-empty, (4) `$HOME/.local/state/ooda-pr` if `$HOME` is set and non-empty, (5) `std::env::temp_dir().join("ooda-pr")` (e.g. `/tmp/ooda-pr` on Linux). Empty env vars are treated as **unset** (a `=""` value falls through). Steps (1) and (2) are used **verbatim** — no `ooda-pr` slug is appended. Steps (3)–(5) append `ooda-pr` because the slug is part of the **state-schema name**, deliberately shared with `/ooda-pr` so per-PR ledgers coexist across both skills. There is no `OODA_PRS_STATE_HOME`; the `OODA_PR_` prefix is the canonical env var.                                                                                                 |
| `--trace PATH`      | Also append the compact trace to PATH. Each PR's `Recorder` opens its own file handle on PATH and appends. Under `--concurrency > 1`, writes from different PRs are not slug-prefixed. POSIX `O_APPEND` gives the **seek-to-end + write** pair atomicity-with-respect-to-other-writers' offset updates (so two writers cannot truncate each other's output via stale offsets), but POSIX does **not** guarantee per-call data atomicity for arbitrary write sizes on regular files — `PIPE_BUF`-bounded no-interleave guarantees apply only to pipes/FIFOs. Rust's `writeln!` typically emits one `write` syscall for line-sized payloads, so adjacent records usually do not splice; long lines or busy concurrent writers can still produce interleave windows. Prefer the per-PR `runs/<run-id>/trace.md` files (one file per PR per run, always written, no cross-PR interleaving) for live attribution. |
| `-h`, `--help`      | Print usage to stdout, exit 0. Pre-scan short-circuits all other validation including flag-repetition checks (`--help --help` exits 0).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |

**Repeating a flag** (`--max-iter`, `--concurrency`,
`--status-comment`, `--state-root`, `--trace`) is a `UsageError`,
**except** `-h` / `--help`, whose pre-scan short-circuits all
parser validation.

## Always-on state

An invocation that **parses successfully** writes per-PR audit
trails and (best-effort) a suite-level audit trail. On `UsageError`
(parser failure) **neither tree is written** — both `Recorder::open`
and `SuiteRecorder::open` are gated on the `Ok(args)` parse branch.

When parsing succeeds:

- The per-PR `Recorder` is required per PR: a recorder-open failure
  for a specific PR turns into that PR's `Outcome::BinaryError`
  and that PR's ledger does not appear (other PRs proceed).
- The suite-level `SuiteRecorder` is best-effort: if its `open`
  fails, `main` prints `warning: suite recorder open failed: …`
  on stderr and proceeds with all per-PR work. A partial-failure
  inside `open` (e.g. manifest write fails after `mkdir -p`) leaves
  the suite directory present but partially populated, not absent.

### Per-PR (shared with `/ooda-pr`)

Same layout as `/ooda-pr`. Keyed by forge + repo + PR, so the same
PR driven from `/ooda-pr` and `/ooda-prs` shares one host-local
memory.

```text
<root>/github.com/<owner>/<repo>/prs/<pr>/
  latest/        index.md, state.json (copy of latest oriented.json),
                 decision.json, blockers.md, next.md, action.json
                 (conditional), outcome.json (conditional)
                 (action.json is present when the latest decision
                  carries an Action; absent on Success / Terminal
                  halts which call remove_latest. outcome.json is
                  written only by
                  record_outcome at the very end of an invocation, so
                  it can be stale or missing if a run aborted before
                  that point.)
  ledger.md      cross-run; one bullet line per event:
                   - <rfc3339-ts> `<run-id>` <summary>
                 (the run-id is wrapped in literal backticks; the
                  hyphen and spacing above are exact)
  ledger.jsonl   cross-run; one JSON object per event:
                   {schema_version, timestamp, run_id, kind, summary}
                 (jsonl carries `kind` and `schema_version`; md is a
                  visualization that drops them — not the same fields)
  events.jsonl   all events, all runs (single file appended-to)
  status-comment/
    dedup.json    JSON object {hash, dedup_key, updated_at}.
                  `hash` is a 16-hex-char FNV-1a 64-bit hash (NOT
                  sha256, NOT cryptographic) of the renderer's
                  `dedup_key` string — chosen for stability across
                  Rust toolchain versions. Written **after** the
                  first successful post in --status-comment mode;
                  absent otherwise. No `rendered.json` /
                  `result.json` siblings here: those artifacts live
                  per-iteration under
                  `runs/<run-id>/iterations/<NNNN>/status-comment/`.
  blobs/sha256/<aa>/<bb>/<hash>.zst                        # zstd-compressed,
                                                           # content-addressed
  runs/<run-id>/
    manifest.json                                          # one per run
    outcome.json  the final per-PR Outcome serialized as JSON,
                  written by record_outcome at run end (the same
                  blob is also copied to ../../latest/outcome.json
                  for the always-on entrypoint)
    trace.md      human-readable run header + per-iter lines
                  (header line is `===== ooda-pr <ts> repo=…` —
                   the literal `ooda-pr` is the per-PR Recorder's
                   schema slug, deliberately shared with `/ooda-pr`;
                   it does NOT mean the run was driven by `/ooda-pr`)
    trace.jsonl   per-run event stream. Each record is one
                  `EventRecord` — the SAME records also appended
                  to the cross-run `events.jsonl` above. Bit-
                  identical content; only file scope differs.
    iterations/<NNNN>/                                     # zero-padded
                                                           # to width 4
      event-range.json    # first/last event sequence numbers for
                          # this iteration (cross-references
                          # events.jsonl entries)
      normalized.json     # raw observe bundle for this iteration
      oriented.json       # OrientedState
      candidates.json     # ranked candidate Vec<Action>
      decision.json       # the chosen Decision
      action.json         # present iff Decision carries an Action
      act-result.json     # present after act() returns
      status-comment/
        rendered.json     # present per iteration when --status-comment
                          # is set (one per record_status_comment_rendered
                          # call); the artifact also gets a content-addressed
                          # copy under blobs/sha256/...
        result.json       # present after each post attempt
      tool-calls/<call-id>/                                # nested under
        stdout.bin, stderr.bin, record.json                # this iteration,
                                                           # not at run_root
```

### Suite-level (new in `/ooda-prs`)

```text
<root>/suites/<suite-id>/
  manifest.json    -- schema_version, suite_id, started_at, forge,
                      mode, max_iter, status_comment, concurrency,
                      cwd, argv, suite (Vec⟨{slug, pr}⟩)
                      Field encodings:
                        forge          : "github.com" (the only currently
                                          supported forge)
                        mode           : "loop" | "inspect" (lowercase
                                          serde rename_all="snake_case")
                        concurrency    : integer ≥ 1, OR `null` when
                                          --concurrency was not given
                                          (the spawn loop resolves null
                                          to the suite size at runtime)
                        status_comment : boolean
  pointers.json    -- schema_version, suite_id,
                      prs : Vec⟨{slug, pr, run_id}⟩
                      Rewritten in full each time a worker calls
                      register_pr. Writers are serialized by an
                      Arc⟨Mutex⟨Inner⟩⟩, but the on-disk write is
                      a non-atomic `fs::write` (open + truncate +
                      write); concurrent external readers may
                      observe a torn or empty file briefly. Final
                      content after all workers register is the
                      complete in-memory `Vec<PrPointer>`.
  outcome.json     -- schema_version, suite_id, finished_at,
                      exit_code, multi_outcome
  trace.md         -- Written at open: a SINGLE-line banner
                        `===== ooda-prs <rfc3339-ts> suite_id=<id> state_root=<path> mode=<loop|inspect> max_iter=<n> status_comment=<bool> concurrency=<n_or_unbounded> =====`
                        (where `<n_or_unbounded>` is the literal
                        decimal `<n>` when `--concurrency` was given,
                        or the bare token `unbounded` (no quotes)
                        when it was not)
                        followed by a blank line, then a single line
                        `Suite: <slug>#<pr>, <slug>#<pr>, …`.
                      In the banner, an unspecified --concurrency
                      renders as the literal string "unbounded",
                      DIVERGING from manifest.json where the same
                      case serializes as JSON `null`.
                      Appended at finalize (after all workers join,
                      only when SuiteRecorder was opened — i.e.
                      parser succeeded): a `## Per-PR results`
                      markdown table with columns
                      `| slug | pr | run_id | outcome | exit |`
                      (one row per PR), then a final
                      `Aggregate exit: **<code>** (started_at=…,
                      finished_at=…)` line with rfc3339 timestamps.
                      (The UsageError code path inside
                      write_trace_summary is structurally unreachable
                      because SuiteRecorder is never opened on parse
                      failure.)
```

`<suite-id>` and per-PR `<run-id>` share one format string,
`{utc:%Y%m%dT%H%M%SZ}-{nanos:09}-p{pid}` (e.g.
`20260505T120000Z-000000000-p1234`): basic-ISO 8601 UTC (no
hyphens or colons), the wall-clock subsecond nanosecond field
(`Utc::now().timestamp_subsec_nanos()`, 9-digit zero-padded — NOT
a process-local counter; uniqueness across rapid opens depends on
clock resolution), and the process pid prefixed with `p`. Inside
a single suite invocation, `Recorder::open` is called concurrently
from K worker threads; collisions on `<run-id>` would require two
threads to obtain the **same** `Utc::now()` instant in the same
process, which is rare on modern systems with high clock resolution
but not formally prevented. Distinct `(slug, pr)` pairs (enforced
by the parser) live under disjoint `<root>/.../prs/<pr>/runs/`
directories, so even a `<run-id>` collision across two threads
would not collide in path because the parent path differs. Two
simultaneous invocations from
distinct processes against overlapping PRs each get a distinct
`<suite-id>` and distinct per-PR `<run-id>`s.

The per-PR `runs/<run-id>/` subtree and the suite-level
`<state-root>/suites/<suite-id>/` subtree are the only fully
collision-free zones across simultaneous invocations on the same
PR. The shared per-PR root has two file classes with different
collision profiles:

- `ledger.md`, `ledger.jsonl`, `events.jsonl` are opened with
  `OpenOptions::append(true)`. Concurrent invocations append; each
  record carries `run_id` so the streams stay disambiguable on
  read. Per-write atomicity for typical line-sized records is
  preserved by the OS, so records do not splice.
- `latest/*.{md,json}` and `status-comment/dedup.json` are written
  via `fs::write` / `fs::copy` (truncate-and-overwrite). These are
  genuinely **last-writer-wins** under simultaneity; readers may
  briefly observe a partial file mid-write.

Same risk profile as `/ooda-pr`; see README invariant `[P8](c)`
for the enumerated sweep.

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

| Condition                                          | `$?` | Mode reachability   |
| :------------------------------------------------- | :--: | :------------------ |
| `MultiOutcome::UsageError`                         |  64  | both (parser-level) |
| any `ProcessOutcome` carries `BinaryError(_)`      |  6   | both                |
| else any carries `HandoffAgent(_)`                 |  5   | both                |
| else any carries `HandoffHuman(_)`                 |  3   | both                |
| else any carries `StuckCapReached(_)`              |  2   | loop only           |
| else any carries `StuckRepeated(_)`                |  1   | loop only           |
| else any carries `WouldAdvance(_)`                 |  4   | inspect only        |
| else every PR ∈ `{DoneMerged, DoneClosed, Paused}` |  0   | both                |

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
{"slug":"acme/widget","pr":1,"outcome":"DoneMerged","exit":0}
{"slug":"acme/widget","pr":2,"outcome":"HandoffAgent","exit":5,"action":"AddressThreads","blocker":"unresolved_threads","prompt":"Address 2 unresolved review threads.\nCopilot: 2 issues.\n\n1. Copilot @ src/foo.rs:42\n   > body line 1\n\n…"}
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
[iter N] halt: <DecisionHaltName>                              -- Success / Terminal halts (no action)
[iter N] halt: <DecisionHaltName> blocker: <BlockerKey>        -- AgentNeeded / HumanNeeded halts
                                                                  (the action's blocker is appended)
```

`<DecisionHaltName>` is one of the literal strings `Success`,
`Terminal(Merged)`, `Terminal(Closed)`, `AgentNeeded`,
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
exit — header line plus optional prompt block for `Handoff*`).

**Inspect mode** runs no iteration loop, so it emits no `[iter N]`
lines at all. What can reach stderr in inspect mode:

- An optional one-shot `stack: <base> → <root>` diagnostic when
  the PR's immediate base branch differs from the resolved stack
  root (suppressed when they match).
- When `--status-comment` is set: a `comment: posted`,
  `comment: skipped (unchanged)`, or `comment: <PostError>` line
  emitted by the post step.
- The final variant block (header + optional prompt block).

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

The names below (`dispatch_subagents_parallel`, `notify_human`,
`escalate_stuck`, `raise_max_iter_or_escalate`,
`triage_binary_error`, `fix_invocation`) are **caller-supplied
placeholders** — `/ooda-prs` does not provide them. Substitute
your harness's own actions.

```
# Loop mode — drive the suite to a halt, dispatch when needed,
# re-invoke until $? = 0. The `[[ -x … ]]` precheck is the only
# reliable guard against cargo-1 failures aliasing Outcome::StuckRepeated.
loop:
  [[ -x ~/.claude/skills/ooda-prs/target/release/ooda-prs ]] || \
    { wrapper_build_failed; break; }
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
    *)  unknown_exit "$?"; break ;;                       # 101 / 128+sig / etc.
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
→ re-invoke) generalizes cleanly: when `$? = 5`, the harness
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
- Each per-PR halt outcome (any of `DoneMerged`, `DoneClosed`,
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
     (per-PR Recorder events, per-PR `runs/<run-id>/trace.md`, the
     PR's own JSONL record). The shared stderr stream is NOT a
     per-PR audit channel under `--concurrency > 1`: lines from
     sibling PRs interleave there without slug prefixes. Per-PR
     semantic preservation holds at the recorder boundary.

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
