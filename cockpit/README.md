# Cockpit

Local web companion UI for OODA agents. Background daemon that
surfaces what your agents are doing on this machine via a browser
at `http://localhost:7777`.

## Run

```bash
~/.claude/skills/cockpit/run serve                # default port 7777
~/.claude/skills/cockpit/run serve --port 9000
~/.claude/skills/cockpit/run serve --bind 127.0.0.1
~/.claude/skills/cockpit/run serve --state-root /custom/path
```

Then open `http://localhost:7777/` in a browser.

### Bind defaults

- **Native Linux / macOS**: defaults to `127.0.0.1` (loopback
  only, no external surface).
- **WSL2** (detected via `WSL_DISTRO_NAME`): defaults to
  `0.0.0.0` so a browser on the Windows host can reach the
  daemon. WSL2's `localhostForwarding` for `127.0.0.1` has been
  historically unreliable; `0.0.0.0` is the no-surprise default.
  Pass `--bind 127.0.0.1` if you've verified your `.wslconfig`
  forwarding works and want loopback-only.

### Reaching the daemon from Windows under WSL2

`http://localhost:7777/` should just work in any Windows browser
once the daemon is running (WSL2 NAT forwards the Windows-side
`localhost` to the WSL2 host's `0.0.0.0` bind). If it doesn't:

- Confirm the daemon is running:
  `curl http://127.0.0.1:7777/api/health` inside WSL2.
- Fall back to the WSL2 IP: `hostname -I` inside WSL2, then visit
  `http://<that-ip>:7777/` from Windows.

## API

- `GET /` — placeholder HTML page (real React frontend lands in
  a follow-up).
- `GET /api/health` — JSON `{ status, state_root, version }`.
- `GET /api/events` — Server-Sent Events stream. One `mutation`
  event per `ooda_state::Event` appended to any active run's
  `events.jsonl`. Payload is the parsed event JSON with an added
  `run_id` field for per-run routing.

Wire shape per SSE event:

```json
{
  "run_id": "20260517T142500Z-123456789-p4242",
  "ts": "2026-05-17T14:25:03Z",
  "kind": "iteration_observed",
  "iteration": 1,
  "blob": { "sha": "…", "size": 4523, "ext": "json" }
}
```

The `kind` discriminant and remaining body fields are exactly the
`ooda_state::Event` serialization; see that crate's docs for the
typed variants.

## State source

Cockpit reads (does not write) the OODA state tree resolved via
the `ooda-state` env chain:

```
$OODA_STATE_HOME
  → $XDG_STATE_HOME/ooda
  → $HOME/.local/state/ooda
  → $TMPDIR/ooda
```

Override with `--state-root PATH`.

State layout (from `ooda-state`):

```
<state-root>/
├── runs/<run-id>/
│   ├── events.jsonl
│   └── blobs/<sha>.<ext>
└── live/<run-id>          # empty marker; presence = "active"
```

## Watcher model

Cockpit runs a single `inotify` watch on `<state-root>/live/`
(non-recursive). That directory has small cardinality (a handful
of active runs at a time), so the watch is cheap.

- On `Create` of `live/<run-id>`: spawn a tail task on
  `runs/<run-id>/events.jsonl` that polls for new bytes and
  broadcasts each parsed event.
- On `Remove` of `live/<run-id>`: cancel the tail task.
- On startup: enumerate existing `live/` markers and start
  tailing each from offset 0; the pre-existing events.jsonl is
  replayed into the broadcast channel once.

The broadcast channel does not replay past messages to late
subscribers — an SSE client connecting mid-run sees only events
appended after its connection. Per-connection backfill of
historical events from disk is future work and will land alongside
the first `GET /api/runs/:run_id/events` endpoint.

Watching the whole state tree was rejected: `runs/` accumulates
one directory per historical run (300k+ on mature machines),
well past the cost-effective range for `inotify`. The `live/`
marker set is the single source of truth for "what's active",
and tailing one file per active run is bounded.

## Status

What's wired:

- HTTP daemon (axum), SSE channel, platform-aware bind.
- `ooda-state` reader: live watcher + per-run tail + broadcast
  fanout.
- Placeholder HTML feed that renders `{run_id, ts, kind, payload}`.

What's deferred:

- **React + Vite frontend**: the placeholder HTML is intentional.
  Real frontend lands as a separate `web/` subdirectory once
  multi-PR navigation, blob preview, and per-run drilldown
  designs are in.
- **Per-OS daemon scripts**: systemd user unit (Linux), launchd
  plist (macOS). Daemon currently runs as `cockpit serve`
  (manual start).
- **Blob fetch endpoint**: `GET /api/runs/:run_id/blobs/:sha`
  lazy-loads the content-addressed payloads referenced by
  events. Not needed for the bare event feed; lands with the
  frontend.
- **Control plane**: POST endpoints to trigger `/ooda-pr`, send
  prompts, pause/resume; WebSocket bidirectional channel.

See `~/.claude/projects/-home-cory-code-skills/memory/project-cockpit-design.md`
for the locked design and the brainstorm/socratic that picked it.

## Architecture

```
~/.local/state/ooda/         ← inotify watch on live/ only
        │
        ▼
   cockpit daemon (axum + tokio + notify)
        │  per-run tail tasks → tokio::broadcast
        │
        ├── GET /              → static HTML (include_str!)
        ├── GET /api/health    → JSON
        └── GET /api/events    → SSE (mutation events)
        │
        ▼
    127.0.0.1:7777 (or 0.0.0.0:7777 under WSL2)
```

URL routing is intentionally **domain-shaped** (`/api/runs/...`
rather than `/api/pr/...`) so the multi-domain mission-control
vision (see `[[project-ooda-multi-domain-vision]]` in memory)
doesn't require a rewrite when domain #2 lands.
