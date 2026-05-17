# ooda-state

Domain-agnostic on-disk model for OODA agents. Writer + reader
crate that every OODA binary writes to, and Cockpit reads from.

See `~/.claude/projects/-home-cory-code-skills/memory/project-ooda-state.md`
for the locked design and the /socratic rationale.

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
construction.**

## Writer protocol

```rust
let root = ooda_state::StateRoot::new("~/.local/state/ooda")?;
let id = ooda_state::RunId::generate();
let mut run = root.create_run(id)?;

// Commit to live index + emit first event
run.start(ooda_state::EventBody::RunStarted {
    domain: "pr".into(),
    target: serde_json::json!({ "slug": "foo/bar", "pr": 42 }),
})?;

// Per-iteration: hash heavy artifacts as blobs, reference from events
let blob = run.write_blob(handoff_md_body, "md")?;
run.append(ooda_state::EventBody::IterationHandoff {
    iteration: 3,
    variant: "HandoffHuman".into(),
    action_kind: "AddressThreads".into(),
    blob,
})?;

// Terminal event + drops live marker
run.halt(ooda_state::EventBody::RunHalted {
    outcome: "HandoffHuman".into(),
    exit_code: 3,
})?;
```

## Reader protocol

```rust
let root = ooda_state::StateRoot::new("~/.local/state/ooda")?;

// Cheap list of active runs
for id in root.live_runs()? {
    let reader = root.open_run(id)?;
    for event in reader.events_stream()? {
        let event = event?;
        // ...
    }
}
```

## Atomicity invariants

- `events.jsonl` appends use `O_APPEND`; lines under `PIPE_BUF`
  (4096 bytes on POSIX) are atomic w.r.t. concurrent readers.
- Blobs written via `tmp + rename` (rename is atomic on the same
  filesystem). Content-addressed → idempotent.
- Live markers use `OpenOptions::create_new` (atomic
  `O_CREAT|O_EXCL`) and `fs::remove_file` (atomic).
- **No locks; no shared mutable state between concurrent runs.**
