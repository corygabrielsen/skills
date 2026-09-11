# ooda-state

Domain-agnostic on-disk model for OODA agents. Writer + reader
crate that every OODA binary writes to, and Cockpit reads from.

## Layout

```text
<state-root>/
├── runs/<run-id>/
│   ├── events.jsonl      ← source of truth (append-only typed events)
│   └── blobs/<sha>.<ext> ← content-addressed payloads
└── live/<run-id>         ← empty marker; presence = "active"
```

No `pr/`, no `<slug>/`, no per-iteration subdirs. Domain semantics
(PR slug, codex-review level, future non-coding identifiers) live
inside `events.jsonl` records via the `target` payload on
`RunStarted` events. **Path-level layout is domain-neutral by
construction.** Binaries may keep sibling directories under the
same root for their own cross-run memory (`index/`, `workspaces/`);
those are outside this crate's contract.

`<run-id>` is opaque to readers. [`RunId::generate`] produces
`<YYYYMMDDTHHMMSSZ>-<entropy>-p<pid>`; the `-p<pid>` suffix is the
liveness witness (see "Liveness"). All directories are created
`0o700`.

## State-root resolution

`resolve_state_root(explicit)` is the one resolution chain every
binary uses:

1. `explicit` (e.g. CLI `--state-root PATH`)
2. `$OODA_STATE_HOME`
3. `$XDG_STATE_HOME/ooda`
4. `$HOME/.local/state/ooda`
5. `$TMPDIR/ooda`

One root per machine, shared by every OODA agent regardless of
domain.

## Event vocabulary

```text
EventBody =
    RunStarted        { domain, target: json }
  ⊕ IterationObserved { iteration, blob }
  ⊕ IterationOriented { iteration, blob }
  ⊕ IterationDecided  { iteration, decision_kind }
  ⊕ IterationHandoff  { iteration, variant, action_kind, blob }
  ⊕ IterationExecuted { iteration, action_kind, success }
  ⊕ IterationWaited   { iteration, action_kind, interval_ms }
  ⊕ RunHalted         { outcome, exit_code }          ← terminal
  ⊕ RunStalled        { last_action }                 ← terminal
  ⊕ RunCapReached     { last_action }                 ← terminal
  ⊕ DomainSpecific    { kind_suffix, payload: json }  ← escape hatch

Event = { ts, body }
```

The string fields are wire tokens, and their literals live in the
`tokens` module rather than in each binary's recorder:

- `OutcomeKind` — the neutral outcome variant names.
- `Domain` trait (`name`, `outcome_token`) with the two production
  impls `PrDomain` and `CodexReviewDomain`, which render
  `DoneSucceeded` as `DoneMerged` and `DoneFixedPoint`
  respectively. Adding a domain means adding an impl here.
- `DecisionKind` — the `decision_kind` tokens on
  `IterationDecided`.
- `DomainKind` — the `kind_suffix` tokens on `DomainSpecific`;
  `domain_specific(kind, payload)` builds the event.
- `terminal_event(domain, kind, exit_code, last_action_kind)` picks
  `RunStalled` / `RunCapReached` for stall- and cap-class outcomes
  and `RunHalted` for everything else, so readers can match on the
  typed terminal variant without parsing the outcome token.
- `blob_path(state_root, run_id, blob)` — the canonical blob
  location for a `BlobRef`.

## Writer protocol

```rust
let root = ooda_state::StateRoot::new(ooda_state::resolve_state_root(None))?;
let id = ooda_state::RunId::generate();
let mut run = root.create_run(id)?;          // runs/<id>/ + blobs/; no live marker yet

// Commit to live index + emit first event
run.start(ooda_state::EventBody::RunStarted {
    domain: "pr".into(),
    target: serde_json::json!({ "slug": "foo/bar", "pr": 42 }),
})?;

// Per-iteration: hash heavy artifacts as blobs, reference from events
let blob = run.write_blob(handoff_md_body.as_bytes(), "md")?;
run.append(ooda_state::EventBody::IterationHandoff {
    iteration: 3,
    variant: "HandoffHuman".into(),
    action_kind: "AddressThreads".into(),
    blob,
})?;

// Terminal event, then drop the live marker
run.halt(ooda_state::tokens::terminal_event(
    &ooda_state::tokens::PrDomain,
    ooda_state::tokens::OutcomeKind::HandoffHuman,
    3,
    None,
))?;
```

Invariants the writer enforces:

- `start` requires `RunStarted` and fails with `AlreadyStarted` if
  a live marker already exists for the id.
- `create_run` fails with `RunDirExists` if `runs/<id>/events.jsonl`
  already has content. Orphan `blobs/*.tmp` from a crashed prior
  writer are swept.
- Every appended line is capped at `MAX_EVENT_BYTES` (4096, POSIX
  `PIPE_BUF`); larger events fail with `EventTooLarge` rather than
  risk a torn append. Heavy payloads go through `write_blob`.
- `write_blob` is idempotent: same bytes + ext reuse the existing
  file.
- `halt` accepts only the three terminal variants. Append-first:
  the terminal event lands on disk before the marker is removed,
  so "absent marker ⇒ terminal event in log" holds for readers. On
  append failure the marker is left in place and `AlreadyHalted`
  guards a second call.
- `Drop` on an un-halted writer emits a `DroppedWithoutHalt`
  fallback and clears the marker, so a panic or early return does
  not leak an active run.

## Reader protocol

```rust
let root = ooda_state::StateRoot::new(ooda_state::resolve_state_root(None))?;

// Cheap list of active runs (PID-liveness filtered)
for id in root.live_runs()? {
    let reader = root.open_run(id)?;
    for event in reader.events_stream()? {
        let event = event?;
        // ...
    }
}
```

- `events` / `events_stream` are lenient: a trailing line without
  `\n` is treated as writer-mid-flight and skipped; malformed
  complete lines are skipped. `events_strict` /
  `events_stream_strict` surface the first parse error instead.
- `read_blob` verifies the SHA-256 against `BlobRef` and refuses
  blobs over `MAX_INLINE_BLOB_SIZE` (64 MiB) with `BlobTooLarge`;
  `read_blob_stream` returns a `HashVerifyingReader` for larger
  payloads (`verify()` after draining).
- `open_run` fails with `UnknownRun` if `runs/<id>/` is absent.

## Liveness

`live/<run-id>` markers can leak across SIGKILL / OOM / power loss.
`live_runs` filters out markers whose embedded PID no longer
answers `kill(pid, 0)`; `live_runs_unfiltered` lists every marker
for diagnostics; `sweep_dead_markers` unlinks the dead ones and
returns their ids. Markers whose id has no parseable PID suffix are
never swept.

## Atomicity invariants

- `events.jsonl` appends use `O_APPEND`; lines under `PIPE_BUF`
  (4096 bytes on POSIX) are atomic w.r.t. concurrent readers, and
  the writer rejects anything longer.
- Blobs written via `tmp + rename` (rename is atomic on the same
  filesystem). Content-addressed → idempotent.
- Live markers use `OpenOptions::create_new` (atomic
  `O_CREAT|O_EXCL`) and `fs::remove_file` (atomic).
- Concurrent runs use disjoint paths, so the layout needs no
  inter-run locking. Each `RunWriter` is a single-threaded `&mut`
  handle, which rules out in-process aliasing by construction.
