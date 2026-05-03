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

Drives one PR to its terminal state. **`$?` must reflect ooda-pr's
exit** — that means: do not pipe (`|`) it, do not background (`&`)
it, and do not put it on the _left_ of `&&` or `||`. ooda-pr on the
right of `&&`/`||`, or as the last command after `;`, is fine
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

| Flag           | Meaning                                                                                                                                                                                                                                                      |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--max-iter N` | Iteration cap. Default 50. Ignored by `inspect`.                                                                                                                                                                                                             |
| `--comment`    | Post a fitness comment to the PR each iteration. Deduped per-PR via local FNV-1a hash file at `/tmp/ooda-pr-<owner>-<repo>-<pr>.hash`; identical-body re-posts are suppressed for as long as `/tmp` survives (typically across the session, lost on reboot). |
| `-h`, `--help` | Print usage to stdout, exit 0.                                                                                                                                                                                                                               |

## What you get back

Stdout/stderr carry human-readable diagnostics. **The exit code is
the decision** — dispatch on `$?`, do not parse stdout.

| Exit | Class          | What it means                                                                                                                                                                                                                                    | What you do next                                                                                                            |
| ---- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| 0    | `success`      | PR reached its target. Stderr distinguishes the sub-case: `Halt: PR merged` / `Halt: PR closed` / `Halt: Success — no advancing actions`.                                                                                                        | Stop driving. (For non-terminal sub-cases, the caller decides whether to re-run later.)                                     |
| 1    | `stalled`      | Same `(kind, blocker)` action fired twice in a row (`kind` = action variant like `AddressThreads`; `blocker` = stable string identifying the underlying issue). State isn't advancing.                                                           | Do not auto-retry. Read stderr for the repeating action; fix the underlying blocker or escalate to the user.                |
| 2    | `cap_reached`  | Iteration cap hit without halting.                                                                                                                                                                                                               | Re-run to continue, or raise `--max-iter`. Two consecutive `cap_reached` on the same PR (caller tracks) ⇒ treat as stalled. |
| 3    | `human_needed` | A human must act (approve, push, …).                                                                                                                                                                                                             | Surface the description on stderr to the caller verbatim. Re-run later.                                                     |
| 4    | `in_progress`  | **`inspect` only.** Decide returned an `Execute` (any automation: `Full`, `Agent`, `Wait`, or `Human`) — the loop would run/dispatch/wait, but the probe halts before acting. `Halt` decisions return their normal class (0/3/5) in inspect too. | Re-invoke without `inspect` to actually drive it.                                                                           |
| 5    | `agent_needed` | An agent must act (address threads, fix CI, …).                                                                                                                                                                                                  | Run an agent with the description on stderr as the prompt. Re-invoke `/ooda-pr` after.                                      |
| 6    | `runtime`      | `gh` subprocess or transport failure (auth, network, missing CLI).                                                                                                                                                                               | Distinct from `stalled`. Retry, or alert on persistent failure.                                                             |
| 64   | `usage`        | Bad arguments.                                                                                                                                                                                                                                   | Fix the invocation. `--help` shows the usage.                                                                               |

The action description on `agent_needed` / `human_needed` halts is
prompt material — surface it verbatim to the caller. Do not
paraphrase. Stderr format (literal — preserve the two-space indent
when grepping):

```
Halt: <Class> — <Kind>
  description: <text on this line, possibly continuing on indented
    lines until a non-indented line or EOF>
```

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
- The same `(kind, blocker)` action fires twice in a row
  (`stalled` — exit 1)
- Iteration cap hit (`cap_reached` — exit 2)

An action's _automation_ is one of `Full`, `Agent`, `Wait`, or
`Human` — printed alongside its kind in iteration logs.
`Wait`-automation actions (e.g. `WaitForCi`, `WaitForBotReview`)
poll external state: the loop sleeps the action's interval, then
re-observes. They do not trip the stall detector. The loop exits
only when the awaited state changes (decide returns a non-`Wait`
halt) or the iteration cap is hit.

## Build

Manual build: `cd ~/.claude/skills/ooda-pr && cargo build --release`.

For the type-level specification see `~/.claude/skills/ooda-pr/README.md`.
