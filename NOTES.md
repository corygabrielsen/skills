# ooda-codex-review cut-over to ooda-state — design decisions

## Run granularity: one invocation = one run

Followed the kickoff default. Each `ooda-codex-review` invocation
generates a fresh `RunId` (via `RunId::generate()`) and writes a
self-contained run into `<state-root>/runs/<run-id>/`. The previous
recorder's multi-invocation accumulator model (one run that persists
across many invocations via a `latest` pointer and resume protocol)
is gone.

Implication: ladder transitions become events WITHIN one run
when they occur inside an OODA-loop iteration, AND can also be
standalone single-event runs when the orchestrator invokes a
side-effect flag like `--advance-level` directly. Both shapes use
the same `EventBody::IterationDecided` / `EventBody::IterationHandoff`
vocabulary; the difference is just how many events appear before
`RunHalted`.

## Domain and target shape on RunStarted

```json
{
  "kind": "run_started",
  "domain": "codex-review",
  "target": {
    "floor": "low",
    "ceiling": "xhigh",
    "mode": "uncommitted|base|commit|pr",
    "value": "<branch|sha|pr-num>?"
  }
}
```

Per kickoff: `domain="codex-review"` (distinct from the three
PR-side binaries), and `target` carries the floor/ceiling ladder
bounds plus the review mode (no PR slug — that's a PR-domain
concept). For PR-mode invocations the resolved base branch is
captured under `target.value` after `gh pr view` resolution, but
the `target` field never identifies "which PR in which repo" —
that level of identity is the PR domain's, not codex-review's.

## What goes in events vs scratch

`events.jsonl` captures the OODA event stream — observations,
orient snapshots, decisions, handoff prompts, executed actions,
and the terminal event. Per the v2 design, large payloads
(observation snapshots, handoff prompt bodies) go through
content-addressed `blobs/<sha>.<ext>` and are referenced by
`BlobRef` inside events.

In-flight codex review subprocess logs (`<level>-<slot>.log`,
`<level>-<slot>.exit`) live in `runs/<run-id>/scratch/` — they
are write targets for spawned children, not after-the-fact
content-addressed snapshots. Observe scans them; if the
invocation halts while subprocesses are still writing, those
scratch artifacts stay on disk attached to the dead run and the
next invocation gets its own fresh scratch dir.

## What's deleted from the old recorder

- Resume protocol (`latest` pointer, `try_resume`, `OpenMode`,
  `FreshReason`)
- Cross-invocation state-dir lock (per-invocation runs are
  isolated by `RunId`; no shared mutable state to protect)
- `RunManifest` (replaced by `events.jsonl`)
- `LevelOutcome` history vec (each outcome is now an event)
- `target_root` / `compute_target_root` / per-target path keys
  (run-id is opaque; domain identity lives in events)
- `RecorderConfig`, `Recorder`, `RecorderError` (collapsed into
  direct `ooda_state::RunWriter` usage)
- Repo-id sha-prefixing for state-dir disambiguation (no longer
  needed; each run is independently keyed)

## What's preserved

- `CodexReasoningLevel::{higher,lower}` ladder primitives (move
  to `main.rs` callers — they're pure math, not recorder state).
- Side-effect CLI surface (`--advance-level`, `--mark-*`, etc.)
  remains; each becomes a single-event-run invocation that
  emits the corresponding `IterationDecided` / `IterationHandoff`
  event and halts.
- All `Outcome` mappings and exit codes (per
  `Don't preserve old state` only refers to on-disk state).

## SKILL.md updates

Per the locked design's handoff `see:` pointer contract update,
SKILL.md is updated to:

- describe the new `events.jsonl` + blob layout
- document `domain="codex-review"` and the new target shape
- drop references to `latest`/`manifest`/resume protocol
- update `--state-root` description
- describe how side-effect invocations become single-event runs

## Skipped, out of scope

- Migrating the OLD on-disk state under `$TMPDIR/ooda-codex-review`
  (kickoff: "Don't try to preserve old state").
- Touching `cockpit/`, `ooda-state`, `ooda-core`, `ooda-pr`,
  `ooda-prs`, `ooda-pr-codex-review`.
