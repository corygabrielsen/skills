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
---

# /ooda-pr

Drives one PR to its terminal state. Single Rust binary, single
process, single-shot from the agent's perspective. **Run it in its
own Bash call** — the exit code is the entire communication channel.

## How to call

```bash
~/.claude/skills/ooda-pr/run [options] <owner/repo> <pr>           # loop
~/.claude/skills/ooda-pr/run inspect [options] <owner/repo> <pr>   # one pass
```

The `run` script builds the release binary on demand
(`cargo build --release` is a fast no-op when up-to-date) and execs
it. Once built, the binary at
`~/.claude/skills/ooda-pr/target/release/ooda-pr` works directly.

| Flag           | Meaning                                                                                            |
| -------------- | -------------------------------------------------------------------------------------------------- |
| `--max-iter N` | Iteration cap. Default 50. Ignored by `inspect`.                                                   |
| `--comment`    | Post a fitness comment to the PR each iteration. Deduped — same structural state does not re-post. |
| `-h`, `--help` | Print usage to stdout, exit 0.                                                                     |

## What you get back

Stdout/stderr carry human-readable diagnostics. **The exit code is
the decision** — dispatch on `$?`, do not parse stdout.

| Exit | Class          | What it means                                                                                               | What you do next                                                                       |
| ---- | -------------- | ----------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| 0    | `success`      | PR reached its target. Either no advancing actions, or terminal (merged / closed).                          | Done. Move on.                                                                         |
| 1    | `stalled`      | Same `(kind, blocker)` action fired twice in a row. State isn't advancing.                                  | Investigate why. The last action and its description are on stderr.                    |
| 2    | `cap_reached`  | Iteration cap hit without halting.                                                                          | Re-run to continue, or raise `--max-iter`.                                             |
| 3    | `human_needed` | A human must act (approve, push, …).                                                                        | Surface the description on stderr to the human verbatim. Re-run later.                 |
| 4    | `in_progress`  | **`inspect` only.** Top decision is an executable action; the loop would auto-run it but the probe did not. | Re-invoke without `inspect` to actually drive it.                                      |
| 5    | `agent_needed` | An agent must act (address threads, fix CI, …).                                                             | Run an agent with the description on stderr as the prompt. Re-invoke `/ooda-pr` after. |
| 6    | `runtime`      | `gh` subprocess or transport failure (auth, network, missing CLI).                                          | Distinct from `stalled`. Retry, or alert on persistent failure.                        |
| 64   | `usage`        | Bad arguments.                                                                                              | Fix the invocation. `--help` shows the usage.                                          |

The action description on `agent_needed` / `human_needed` halts is
prompt material — surface it verbatim to the resolver. Do not
paraphrase.

## When to use which mode

- **Default (loop)** — what you normally want. Drives until halt.
  Halts include success and external handoffs (agent / human),
  which the outer driver re-invokes after resolving.
- **`inspect`** — diagnostic. One pass. Use to ask "what would
  ooda-pr do right now?" without side effects. Exit code is the
  same decision class the loop would produce on its first
  iteration. **Use this when you want a probe, not a driver.**

## Loop semantics

The loop runs `(observe ∘ orient ∘ decide ∘ act)*` until one of:

- `decide` returns a halt (success / terminal / agent / human)
- The same `(kind, blocker)` action fires twice without observable
  state change (stall — exit 1)
- Iteration cap hit (exit 2)

`Wait` actions are **expected** to repeat (polling external state)
and do not trip the stall detector.

## Per-bot reports

Copilot and Cursor each have their own typed report. Absence of
signal (`None`) is structurally distinct from low signal
(`Some(report)` with `Idle` activity) — a configured-but-dormant
bot does not block a green PR.

## Build

The `run` wrapper builds on demand. To build manually:

```bash
cd ~/code/skills/ooda-pr && cargo build --release
```

Binary lands at `~/code/skills/ooda-pr/target/release/ooda-pr`.

For the type-level specification see `README.md`.
