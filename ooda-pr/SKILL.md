---
name: ooda-pr
description: Drive a PR through observe → orient → decide → act until merge or external resolution. Default mode loops until halt; `inspect` runs one pass and exits with the decision class encoded as an exit code.
args:
  - name: <owner/repo> <pr>
    description: PR to drive
  - name: inspect
    description: One observe/orient/decide pass; print decision and exit. No act, no loop.
  - name: --max-iter N
    description: Iteration cap (default 50; ignored by inspect)
  - name: --comment
    description: Post a fitness comment on the PR each iteration (deduped)
  - name: -h, --help
    description: Print usage to stdout, exit 0
---

# /ooda-pr

Drives one PR until it is merged, closed, or has no advancing
actions remaining. **`$?` must reflect ooda-pr's exit** — that
means: do not pipe (`|`) it, do not background (`&`) it, and do
not put it on the _left_ of `&&` or `||`. ooda-pr on the right
of `&&`/`||`, or as the last command after `;`, is fine
(`$?` reflects the last command). To capture stderr, redirect
(`2>file`); never pipe. Separate Bash calls in the same turn are
fine.

## How to call

```bash
~/.claude/skills/ooda-pr/run [options] <owner/repo> <pr>           # loop
~/.claude/skills/ooda-pr/run inspect [options] <owner/repo> <pr>   # one pass
```

The `run` script builds the release binary on demand
(`cargo build --release` is a fast no-op when up-to-date) and execs
it. Always invoke `run`, not the built binary at
`~/.claude/skills/ooda-pr/target/release/ooda-pr` directly — the
binary path can serve a stale build silently after source edits.

| Flag           | Meaning                                                                                                                                                                                                                                                                                  |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--max-iter N` | Iteration cap. Default 50. Ignored by `inspect`.                                                                                                                                                                                                                                         |
| `--comment`    | Post a fitness comment to the PR each iteration. Deduped per-PR via local FNV-1a hash file at `/tmp/ooda-pr-<owner>-<repo>-<pr>.hash`; identical-body re-posts are suppressed across runs until `/tmp` is cleared (typically a reboot, or sooner on systems with `tmpfs`-backed `/tmp`). |
| `-h`, `--help` | Print usage to stdout, exit 0.                                                                                                                                                                                                                                                           |

## What you get back

Stdout/stderr carry human-readable diagnostics. **The exit code is
the decision** — dispatch on `$?`, do not parse stdout.

| Exit | Class          | What it means                                                                                                                                                                                                                                                                                                    | What you do next                                                                                                                                                              |
| ---- | -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0    | `success`      | PR is at its target state. Stderr distinguishes which: `Halt: PR merged` (PR-terminal), `Halt: PR closed` (PR-terminal), or `Halt: Success — no advancing actions` (PR open, no work pending — may resume if state changes).                                                                                     | Stop driving. PR-terminal sub-cases are final. PR-open sub-case can be re-run later if PR state may have changed.                                                             |
| 1    | `stalled`      | Same non-`Wait` `(kind, blocker)` action fired twice in a row (`kind` = action variant like `AddressThreads`; `blocker` = stable string identifying the underlying issue). `Wait`-automation actions are exempt; see Loop semantics. State isn't advancing.                                                      | Do not auto-retry. Read stderr for the repeating action; fix the underlying blocker or escalate to the user.                                                                  |
| 2    | `cap_reached`  | Iteration cap hit without halting.                                                                                                                                                                                                                                                                               | Re-run to continue, or raise `--max-iter`. If a re-run also returns 2 on the same PR (caller tracks across invocations), apply the `stalled` (1) response: stop and escalate. |
| 3    | `human_needed` | A human must act (approve, push, …).                                                                                                                                                                                                                                                                             | Surface the description on stderr to the caller verbatim. Re-invoke `/ooda-pr` after the human resolves it.                                                                   |
| 4    | `in_progress`  | **`inspect` only.** Decide returned an `Execute` action with `Full` or `Wait` automation — the loop would auto-run or sleep+poll, but `inspect` halts before acting. `Agent` automation always halts with `agent_needed` (5) and `Human` always halts with `human_needed` (3) in both modes; see Loop semantics. | Re-invoke without `inspect` to actually drive it.                                                                                                                             |
| 5    | `agent_needed` | An agent must act (address threads, fix CI, …).                                                                                                                                                                                                                                                                  | Run an agent with the description on stderr as the prompt. Re-invoke `/ooda-pr` after.                                                                                        |
| 6    | `runtime`      | `gh` subprocess or transport failure (auth, network, missing CLI).                                                                                                                                                                                                                                               | Distinct from `stalled`. Retry, or alert on persistent failure.                                                                                                               |
| 64   | `usage`        | Bad arguments.                                                                                                                                                                                                                                                                                                   | Fix the invocation. `--help` shows the usage.                                                                                                                                 |

The first stderr line is the halt header. The format depends on
the halt class:

`<ActionKind>` below means `action.kind` rendered as its variant name (e.g. `Rebase`, `AddressThreads`).

| Halt class                  | Exit | Header format                                                    |
| --------------------------- | ---- | ---------------------------------------------------------------- |
| `Halt(Success)`             | 0    | `Halt: Success — no advancing actions`                           |
| `Halt(Terminal(Merged))`    | 0    | `Halt: PR merged`                                                |
| `Halt(Terminal(Closed))`    | 0    | `Halt: PR closed`                                                |
| `Halt(AgentNeeded(action))` | 5    | `Halt: AgentNeeded — <ActionKind>`                               |
| `Halt(HumanNeeded(action))` | 3    | `Halt: HumanNeeded — <ActionKind>`                               |
| Loop-level `Stalled`        | 1    | `Halt: Stalled`                                                  |
| Loop-level `CapReached`     | 2    | `Halt: CapReached — last action: Some(<ActionKind>)` (or `None`) |

For `agent_needed` / `human_needed` halts the action description
is prompt material — surface it verbatim to the caller, do not
paraphrase. It follows the header as one or more two-space-indented
lines, the first prefixed `description:`. The block ends at the
next non-indented line or EOF. Preserve the indent when grepping.

```
Halt: AgentNeeded — Rebase
  description: Rebase onto the latest base branch
```

## When to use which mode

- **Default (loop)** — what you normally want. Drives until halt.
  External-handoff halts (`agent_needed`, `human_needed`) are
  re-invoked by the caller after the handoff is resolved;
  `success` does not need re-invocation unless the caller expects
  PR state to have changed (PR-open sub-case only).
- **`inspect`** — diagnostic. One pass. Use to ask "what would
  ooda-pr do right now?" without side effects. Same exit-code
  taxonomy as the loop, plus `in_progress` (4) for executable
  actions the loop would auto-run. **Use this when you want a
  one-pass diagnostic, not a driver.**

## Loop semantics

The loop runs `(observe ; orient ; decide ; act)*`. Each
iteration, `decide` selects a candidate action and inspects its
`automation` field. The field determines whether `decide` wraps
the action in `Execute(action)` (dispatched to `act`) or in a
`Halt(...)` (loop exits):

| Automation | `decide` wraps as           | Loop behavior                                        |
| ---------- | --------------------------- | ---------------------------------------------------- |
| `Full`     | `Execute(action)`           | `act` runs the action directly; loop continues       |
| `Wait`     | `Execute(action)`           | `act` sleeps the action's interval; loop re-observes |
| `Agent`    | `Halt(AgentNeeded(action))` | Loop exits with `agent_needed` (5)                   |
| `Human`    | `Halt(HumanNeeded(action))` | Loop exits with `human_needed` (3)                   |

The `Halt` variants `Success` (no advancing actions, including
PR open with nothing to do) and `Terminal(Merged | Closed)` exit
with `success` (0) directly from `decide`, without an action.

The loop terminates on:

- A `Halt` decision (`success`, `agent_needed`, or `human_needed`)
- The same non-`Wait` `(kind, blocker)` action fires twice in a
  row (`stalled` — exit 1). `Wait`-automation actions
  (e.g. `WaitForCi`, `WaitForBotReview`) are exempt: poll-and-
  repeat is their job.
- Iteration cap hit (`cap_reached` — exit 2)
- A `gh` subprocess or transport error (`runtime` — exit 6).
  Distinct from a halt: the loop _failed_ rather than _completed_.

## Build

Manual build: `cd ~/.claude/skills/ooda-pr && cargo build --release`.

For deeper semantics — the type-level algebra (`Decision`,
`Action`, `Automation`, the orient axes, the named invariants) —
see `~/.claude/skills/ooda-pr/README.md`. The caller does not
need to consult it for normal use; this SKILL is self-sufficient.
