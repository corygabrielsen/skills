---
name: ooda-pr
description: PR-specific OODA loop. Drives a PR through observe → orient → decide → act until merge or external resolution. Replaces /converge + /pr-fitness.
args:
  - name: <owner/repo> <pr>
    description: PR to drive
  - name: --once
    description: One observe/orient/decide pass; print decision and exit. No act, no loop.
  - name: --max-iter N
    description: Iteration cap (default 50)
---

# /ooda-pr

Compiled Rust binary. Single binary, single process, no JSON
boundary. Run as a **single command in its own Bash call** — the
exit code is the communication channel.

```bash
~/.claude/skills/ooda-pr/run [opts] <owner/repo> <pr>
```

The `run` script builds the release binary on demand
(`cargo build --release` is a fast no-op when up-to-date) and execs
it. `target/` is gitignored so the wrapper is required on fresh
installs; the binary path
`~/.claude/skills/ooda-pr/target/release/ooda-pr` works directly
once built.

## Modes

`--once` runs one observe → orient → decide pass and prints the
top decision. No `act`, no loop. Use for inspection — same role
as `pr-fitness <repo> <pr>` had.

Default mode runs the full loop: observe → orient → decide → act
→ repeat. Halts on success (PR at target), terminal lifecycle
(merged/closed), agent or human handoff, stall (same action twice
in a row), or iteration cap.

## Halt taxonomy (by exit code)

| Exit | Status       | What it means / agent response                                                                                          |
| ---- | ------------ | ----------------------------------------------------------------------------------------------------------------------- |
| 0    | success      | PR reached its target — no advancing actions, or terminal (merged/closed).                                              |
| 1    | stalled      | Same (kind, blocker) action fired twice in a row. State isn't advancing.                                                |
| 2    | cap_reached  | Iteration cap hit without halting. Re-run to continue, or raise --max-iter.                                             |
| 3    | human_needed | Action requires a human (approve, push, …). Surface description; re-run later.                                          |
| 4    | in_progress  | `--once` only: top decision is an executable action (Wait/Full). The full loop would auto-run it; the probe did not.    |
| 5    | agent_needed | Action requires an agent (address threads, fix CI, …). Run agent and re-invoke.                                         |
| 6    | runtime      | `gh` subprocess or transport failure (auth, network, missing CLI). Distinct from `stalled` so wrappers can retry/alert. |
| 64   | usage        | Bad arguments. Fix invocation.                                                                                          |

The action description on `agent_needed` / `human_needed` halts
is the prompt material — surface it verbatim to the resolver.

## What's different vs /converge + /pr-fitness

- **One binary, no JSON pipe.** observe → orient → decide → act
  all live in process. Decide consumes typed Rust structs from
  orient, no string parsing or schema versioning.
- **No combined bot tier scalar.** Halt is a predicate over the
  candidate-action set, not a `score >= target` comparison.
  Idle bots emit no candidates and contribute nothing —
  structurally rules out the false-stall bug class where a
  configured-but-dormant Copilot capped a green PR at bronze.
- **Per-bot state preserved.** Copilot and Cursor each have their
  own report; absence-of-signal stays distinct from low-signal.
- **Parallel observe.** Ten GitHub fetchers fan out via
  `std::thread::scope`. End-to-end ~3-4s per iteration on a
  typical PR.

## When to use

- Use `/ooda-pr` for any PR convergence task once it's installed.
- The old `/converge /pr-fitness …` invocation still works and
  remains the fallback while ooda-pr is being shaken out.

## Build

```bash
cd ~/code/skills/ooda-pr && cargo build --release
```

The release binary is at `~/code/skills/ooda-pr/target/release/ooda-pr`.
