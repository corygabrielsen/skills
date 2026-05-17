# ooda-pr-codex-review → ooda-state cutover

Task #227. Mapping decisions and rationale.

## Domain

`domain = "pr"`. The codex-review axis is one of nine axes on a PR
domain; it does not change the binary's domain. The standalone
`ooda-codex-review` binary lives in a different domain.

## RunStarted.target shape

```json
{
  "slug": "owner/repo",
  "pr": 42,
  "mode": "loop" | "inspect",
  "max_iter": 50,
  "status_comment": false,
  "codex_review": null | {
    "floor": "low",
    "ceiling": "high",
    "n": 3
  }
}
```

`codex_review = null` ⇔ axis disabled (CLI ceiling = off). Non-null
captures the per-invocation ladder bounds + parallelism.

## Event mapping

| Old recorder method                | New event                                                         |
| ---------------------------------- | ----------------------------------------------------------------- |
| `open` (run start)                 | `RunStarted { domain:"pr", target }`                              |
| `record_observe_start`             | `DomainSpecific { kind_suffix:"observe_started", payload }`       |
| `record_observe_end`               | `DomainSpecific { kind_suffix:"observe_finished", payload }`      |
| `record_iteration` observe blob    | `IterationObserved { iteration, blob:normalized.json }`           |
| `record_iteration` oriented        | `IterationOriented { iteration, blob:oriented.json }`             |
| `record_iteration` decision        | `IterationDecided { iteration, decision_kind }` + 3 blob events   |
| `record_action_start`              | `DomainSpecific { kind_suffix:"action_started", payload }`        |
| `record_action_end`                | `IterationExecuted { iteration, action_kind }`                    |
| `record_wait_start`                | `DomainSpecific { kind_suffix:"wait_started", payload }`          |
| `record_wait_end`                  | `IterationWaited { iteration, action_kind, interval_ms }`         |
| `record_status_comment_*`          | `DomainSpecific { kind_suffix:"status_comment_*", payload }`      |
| `tool_call_*`                      | `DomainSpecific { kind_suffix:"tool_call_*", payload+blob refs }` |
| `record_outcome` (success/handoff) | `RunHalted { outcome, exit_code }`                                |
| `record_outcome` (StuckRepeated)   | `RunStalled { last_action }`                                      |
| `record_outcome` (StuckCapReached) | `RunCapReached { last_action }`                                   |
| `write_handoff_md`                 | `write_blob(prompt, "md")` → returns blob path                    |
| `write_trace_line`                 | `DomainSpecific { kind_suffix:"trace_line", payload:{line} }`     |

Blobs (`normalized`, `oriented`, `candidates`, `decision`,
`dashboard`, `index.md`, `blockers.md`, `next.md`, handoff body,
tool stdout/stderr/record) are written via
`RunWriter::write_blob`; events carry `BlobRef { sha, size, ext }`.

## Codex axis state (the ambiguous part)

The codex axis has two kinds of state:

1. **Observations** — `BatchState` / `VerdictRecord` lists per
   ladder level per head SHA. **Maps cleanly**: emitted as
   `DomainSpecific { kind_suffix:"codex_observed", payload:{level,
batch_state, blob_refs_to_log_files} }` events on each
   iteration.

2. **Spawn workspace** — physical directory where `codex review`
   subprocesses write `<L>-<slot>.log`, `<L>-<slot>.exit`,
   `head_sha.txt`. This is shared act/observe state with two
   constraints the ooda-state model doesn't satisfy:
   - **Cross-run persistence** (cache survives across runs of the
     binary against the same PR).
   - **Stable PR-keyed path** (act-side spawn writes there; the
     same path must be observable in the next iteration).

   The ooda-state model is run-opaque and per-iteration immutable;
   spawn workspace is the opposite shape.

### Decision: split the workspace out of the recorder

Codex spawn workspace lives at a **separate, PR-keyed path
outside `<state-root>/runs/`**:

```
<state-root>/workspaces/pr-codex-review/<slug>/<pr>/
  .lock                       advisory flock (lifetime of run)
  levels/<L>/<head_sha[:12]>/
    head_sha.txt
    <L>-<slot>.log
    <L>-<slot>.exit
```

This is **act-side state**, not recorder state. The recorder emits
DomainSpecific events that reference the log files via blob refs
(snapshots taken at observation time), so the immutable per-run
audit trail in `events.jsonl` is complete on its own — the
workspace is just the act-side scratch.

A symmetric `<state-root>/workspaces/pr-comment-dedup/<slug>/<pr>.json`
replaces the old `<pr_root>/status-comment/dedup.json` for the
status-comment dedup state (also cross-run PR-keyed).

Rationale: the new ooda-state design explicitly carves out
"domain-specific index (additive, per-binary)" as an extension
point for per-domain caches that the run-opaque core deliberately
doesn't cover. The codex workspace and the comment dedup file
are exactly that shape.

### Alternatives considered

- **Put codex logs inside the run** — breaks the cache property
  (next run can't reuse them).
- **Put codex logs at a path keyed on `RunWriter::run_id`** —
  same problem: per-run dir is scoped to one process.
- **Snapshot codex logs as content-addressed blobs only** — works
  for the audit trail but breaks the act-side spawn protocol
  (codex needs a fixed directory to write into).

## Things deleted

- `recorder.rs::CurrentManifest` writes — `CURRENT.json` is gone
  from the new model; readers walk `events.jsonl` instead.
- `recorder.rs::publish_current`, `publish_current` artifacts map,
  `outcome_has_action` predicate.
- `ledger.jsonl` / `ledger.md` cross-run streams — superseded by
  per-run `events.jsonl`; cross-run browse walks `runs/`.
- `trace.md` / `trace.jsonl` per-run files — superseded by
  `events.jsonl` (DomainSpecific `trace_line` events).
- `manifest.json` per-run file — superseded by `RunStarted` event
  payload.
- `event-range.json` per-iteration file — was a derived index;
  `events.jsonl` is the source of truth.
- `tools-calls/*` per-iteration directory — tool call bytes now
  live as blobs referenced from `tool_call_finished` events.
- `--legacy-trace` / `RecorderConfig::legacy_trace` — superseded
  by event log; `--trace PATH` flag is retained as a noop tail
  alias (writes to the same path it always did, derived from
  events).

  → Decided not to retain the flag-noop; the flag is removed
  too. `--trace PATH` is no longer a CLI arg.

## SKILL.md changes

The "Always-On State" section's filesystem layout block is
replaced with the new `<state-root>/runs/<id>/events.jsonl + blobs/`
shape; the "Recorder layout (codex sub-tree)" section is replaced
with the workspaces shape; the `--trace PATH` flag row is removed.

The Handoff\* `see:` pointer now targets a blob inside
`runs/<run-id>/blobs/<sha>.md`, not a per-iteration `handoff.md`.

## Integration test

`tests/cli.rs::state_root_records_even_when_observe_fails` is
rewritten to assert the new event shapes (`events.jsonl` exists;
contains `run_started` / `run_halted` events with `domain="pr"`
target; `live/<id>` marker is absent after halt).
