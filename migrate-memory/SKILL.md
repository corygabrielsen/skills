---
name: migrate-memory
description: Push the current session's accumulated memory to a target project dir before spawning a fresh claude session there. One-shot snapshot; no live sync; no symlink.
---

# /migrate-memory

Push the current session's accumulated memory
(`~/.claude/projects/<slug>/memory/`) to a target project dir before spawning
a fresh `claude` session there. One-shot snapshot. No live sync. No symlink.

Use this when you must spawn `claude` in a different cwd than the current
session (typically to pick up a deeper CLAUDE.md / `@`-included AGENTS.md tree
that Claude Code only walks at process spawn) and want your accumulated memory
to come with you instead of starting empty.

## Use

```
/migrate-memory <target-cwd>             # refuse on conflict
/migrate-memory <target-cwd> --force     # overwrite target's memories
/migrate-memory <target-cwd> --dry-run   # show what would copy, do nothing
/migrate-memory <target-cwd> --source <abs-path>  # override source autodetect
```

`<target-cwd>` is an absolute path to the directory where you intend to spawn
`claude` next. The skill slugifies it (`/` → `-`) and copies the current
session's `memory/` subdir to `~/.claude/projects/<slug>/memory/`.

## Run

Invoke the wrapper directly via Bash; surface its stdout and exit code:

```
~/.claude/skills/migrate-memory/migrate.sh <target-cwd> [flags]
```

## Source autodetect

Order: `--source <path>` → `$CLAUDE_PROJECT_DIR` → `$PWD`. Print the resolved
source before doing anything so the user can interrupt if wrong.

## Out of scope

- Conversation transcripts (`*.jsonl`) — per-session by design
- `runs/`, `status-comment/`, `blobs/` — keyed on forge+repo+PR; already
  shared across project dirs via the state-root resolution chain
- Pull mode (target → source), periodic sync, per-file merge UI

## Exit codes

| Code | Meaning                     |
| ---- | --------------------------- |
| 0    | Success or dry-run complete |
| 1    | Source has no memory files  |
| 2    | Conflict without `--force`  |
| 64   | Bad arguments               |
