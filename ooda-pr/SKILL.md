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

Drives one PR to its terminal state. **Do not chain it inside a
single Bash tool call** — any of `&&`, `||`, `|`, `;`, `&` corrupts
the exit code, breaking the dispatch contract. (Separate Bash calls
in the same turn are fine.) To capture stderr, redirect (`2>file`);
never pipe.

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

| Flag           | Meaning                                                                                                                                                         |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--max-iter N` | Iteration cap. Default 50. Ignored by `inspect`.                                                                                                                |
| `--comment`    | Post a fitness comment to the PR each iteration. Deduped per-PR via FNV-1a hash on the rendered body, persisted via GitHub comment history (survives restarts). |
| `-h`, `--help` | Print usage to stdout, exit 0.                                                                                                                                  |

## What you get back

Stdout/stderr carry human-readable diagnostics. **The exit code is
the decision** — dispatch on `$?`, do not parse stdout.

| Exit | Class          | What it means                                                                                                                                                                                                      | What you do next                                                                                                            |
| ---- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| 0    | `success`      | PR reached its target. Either no advancing actions (PR open, just paused), or terminal (merged / closed). Discriminate via stderr: `Halt: PR merged` / `Halt: PR closed` / `Halt: Success — no advancing actions`. | Stop driving. If terminal, done. If non-terminal, re-run later when state may have changed.                                 |
| 1    | `stalled`      | Same `(kind, blocker)` action fired twice in a row. State isn't advancing.                                                                                                                                         | Do not auto-retry. Read stderr for the repeating action; fix the underlying blocker or escalate to the user.                |
| 2    | `cap_reached`  | Iteration cap hit without halting.                                                                                                                                                                                 | Re-run to continue, or raise `--max-iter`. Two consecutive `cap_reached` on the same PR (caller tracks) ⇒ treat as stalled. |
| 3    | `human_needed` | A human must act (approve, push, …).                                                                                                                                                                               | Surface the description on stderr to the caller verbatim. Re-run later.                                                     |
| 4    | `in_progress`  | **`inspect` only.** Top decision is an executable action; the loop would auto-run it but the probe did not.                                                                                                        | Re-invoke without `inspect` to actually drive it.                                                                           |
| 5    | `agent_needed` | An agent must act (address threads, fix CI, …).                                                                                                                                                                    | Run an agent with the description on stderr as the prompt. Re-invoke `/ooda-pr` after.                                      |
| 6    | `runtime`      | `gh` subprocess or transport failure (auth, network, missing CLI).                                                                                                                                                 | Distinct from `stalled`. Retry, or alert on persistent failure.                                                             |
| 64   | `usage`        | Bad arguments.                                                                                                                                                                                                     | Fix the invocation. `--help` shows the usage.                                                                               |

The action description on `agent_needed` / `human_needed` halts is
prompt material — surface it verbatim to the caller. Do not
paraphrase. On stderr it is the indented line(s) under the
`Halt: <Class> — <Kind>` header, prefixed ` description:`.

## When to use which mode

- **Default (loop)** — what you normally want. Drives until halt.
  Halts include `success` and external handoffs (`agent_needed` /
  `human_needed`), which the caller re-invokes after resolving.
- **`inspect`** — diagnostic. One pass. Use to ask "what would
  ooda-pr do right now?" without side effects. Same exit-code
  taxonomy as the loop, plus `in_progress` (4) for executable
  actions the loop would auto-run. **Use this when you want a
  probe, not a driver.**

## Loop semantics

The loop runs `(observe ; orient ; decide ; act)*` until one of:

- `decide` returns a halt — `success` (terminal merge / close folds
  in here), `agent_needed`, or `human_needed`
- The same `(kind, blocker)` action fires twice without observable
  state change (`stalled` — exit 1)
- Iteration cap hit (`cap_reached` — exit 2)

An action's _automation_ is one of `Full`, `Agent`, `Wait`, or
`Human` — printed alongside its kind in iter logs.
`Wait`-automation actions (e.g. `WaitForCi`, `WaitForBotReview`)
are **expected** to repeat — they poll external state — and do
not trip the stall detector.

## Build

Manual build: `cd ~/.claude/skills/ooda-pr && cargo build --release`.

For the type-level specification see `README.md`.
