# Cockpit

Local web companion UI for OODA agents. Background daemon that
surfaces what your agents are doing on this machine via a
browser at `http://127.0.0.1:7777`.

## Run

```bash
~/.claude/skills/cockpit/run serve              # default port 7777
~/.claude/skills/cockpit/run serve --port 9000
~/.claude/skills/cockpit/run serve --state-root /custom/path
```

## API (V1)

- `GET /` — placeholder HTML page (real React frontend lands in
  a follow-up).
- `GET /api/health` — JSON `{ status, state_root, version }`.
- `GET /api/events` — Server-Sent Events stream. V1 emits a
  `heartbeat` event every 5s; once the file-watcher is wired,
  the same channel emits `mutation` events for state-tree
  changes.

## State source

Cockpit reads (does not write) the OODA state tree resolved via
the same env chain `ooda-pr` uses:

```
$OODA_PR_STATE_HOME
  → $XDG_STATE_HOME/ooda-pr
  → $HOME/.local/state/ooda-pr
  → $TMPDIR/ooda-pr
```

Override with `--state-root PATH`.

## Status

V1 = daemon shell. End-to-end pipeline works: HTTP server,
placeholder HTML, health endpoint, SSE stream.

What's deferred:

- **File-watcher**: a naive recursive watch on the full state
  tree is unworkable — mature trees hit ~300k+ directories
  (one per immutable iteration), well past the cost-effective
  range for either `inotify` (per-handle setup) or
  `PollWatcher` (per-poll walk). The mutation feed needs a
  scope-narrowing policy first (watch only recently-touched
  PRs? Project a small set of canonical files like
  `CURRENT.json`? Server-side polling of a curated set?). V1.1
  will pick.
- **React + Vite frontend**: the placeholder HTML is
  intentional. Real frontend lands as a separate `web/`
  subdirectory once the V1.1 watcher gives it real data to
  render.
- **Per-OS daemon scripts**: systemd user unit (Linux),
  launchd plist (macOS). V1 ships as `cockpit serve` (manual
  start); auto-start is V1.2.
- **Control plane**: V2. POST endpoints to trigger
  `/ooda-pr`, send prompts, pause/resume. WebSocket
  bidirectional channel comes with it.

See `~/.claude/projects/-home-cory-code-skills/memory/project-cockpit-design.md`
for the locked design and the brainstorm/socratic that picked it.

## Architecture

```
~/.local/state/ooda-pr/         ← (V1.1+) PollWatcher / scoped notify
        │
        ▼
   cockpit daemon (axum + tokio)
        │
        ├── GET /              → static HTML (include_str!)
        ├── GET /api/health    → JSON
        └── GET /api/events    → SSE (heartbeat in V1; mutations in V1.1)
        │
        ▼
    127.0.0.1:7777
```

URL routing is intentionally **domain-shaped**:
`/api/runs/...` rather than `/api/pr/...`, so the multi-domain
mission-control vision (see `[[project-ooda-multi-domain-vision]]`
in memory) doesn't require a rewrite when domain #2 lands.
