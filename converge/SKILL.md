---
name: converge
description: Observe→decide→act loop. Halts on target, stall, cap, or human/agent delegation.
args:
  - name: "-- <command>"
    description: Fitness command (everything after --)
  - name: -n, --max-iter
    description: Iteration ceiling (default 20)
  - name: -s, --session
    description: Session identifier (default hash of command)
  - name: --hook
    description: Coprocess for progress events (receives JSONL on stdin)
---

# /converge

Compiled Rust binary. Run as a **single command in its own Bash
call**. Do not chain with `&&`, `;`, or pipes — the exit code is
the communication channel.

```bash
~/code/skills/converge/target/release/converge [opts] -- <command> [args...]
```

Everything after `--` is the fitness command. converge spawns it
repeatedly, reads a JSON fitness report from its stdout, and
dispatches prescribed actions until the target score is reached.

The CLI prints the session directory to stderr on startup. Read
`exit.json` from that directory for the structured halt report.

## Halt taxonomy (by exit code)

| Exit | Status                | Agent response                                                                                        |
| ---- | --------------------- | ----------------------------------------------------------------------------------------------------- |
| 0    | `success`             | Target reached. Check `structural_blockers` in exit.json — if present, resume when they clear.        |
| 1    | `stalled`             | No advancing actions. May resolve externally; retry later or escalate.                                |
| 2    | `timeout`             | Iteration or poll cap reached. Re-running continues iteration numbering. If no progress, investigate. |
| 3    | `hil`                 | Human action required. Surface `action.description` from exit.json to the user. Wait, then re-run.    |
| 4    | `error`               | Inspect `cause` in exit.json. Retry if transient; investigate if persistent.                          |
| 5    | `agent_needed`        | Agent task required. See resume instructions below.                                                   |
| 6    | `terminal`            | Subject reached terminal state. Check `terminal.kind` in exit.json. No action needed.                 |
| 7    | `cancelled`           | SIGINT/SIGTERM received. Safe to re-run; session state is preserved.                                  |
| 8    | `fitness_unavailable` | Fitness command unreachable. Check `cause` in exit.json. No `final_score` available. Retry later.     |
| 9    | (lock held)           | Another session owns the lock. No exit.json written. Wait and retry.                                  |
| 64   | (usage)               | Bad arguments. No exit.json written. Fix invocation.                                                  |

When exit.json exists, check `stage === "final"` before reading
status — `"in_progress"` means converge is still running. Exit
codes 9 and 64 produce no exit.json.

## Score

`score` and `target` are numeric scalars emitted by the fitness
command. converge halts `success` when `score >= target`. It
compares them numerically but is agnostic about what the scalar
represents. The exit.json field is `final_score`.

## Blocker categories

The fitness report may include `blocker_split` with three categories.
These are **fitness-command conventions** — converge reads
`structural` from `blocker_split` for halt metadata, and uses the
flat `blockers` list for iteration-key boundaries (blocker changes
advance the iter):

- **agent** — the fitness command uses these to cap the score.
- **human** — informational. The fitness command may emit actions
  with `automation: "human"`, which converge halts as `hil`. The
  `blocker_split.human` list itself does not trigger halts.
- **structural** — when `score >= target` and `structural` is
  non-empty, the `success` halt carries `structural_blockers`.

The fitness command defines what each category means for its domain.

## Hook

The `--hook` flag spawns a long-running coprocess. converge sends
newline-delimited JSON events to its stdin:

```jsonl
{"event":"iteration","iter":1,"report":{...},"action":{...}}
{"event":"halt","halt":{...},"last_report":{...}}
```

The hook processes events asynchronously. converge does not wait
for the hook — delivery is ordered by the stdin stream. If no
`--hook` is specified, no coprocess is spawned.

## Resume

### On `agent_needed` (exit 5)

1. Read exit.json: `action` has the delegated task.
2. Perform the task in `action.description` (and `action.context`
   if present).
3. Re-run the same converge command — session continues from
   `history.jsonl`.

### On structural halt (exit 0 with `structural_blockers`)

Re-run the same command when the blocking condition clears.

## Example: converge with pr-fitness

```bash
~/code/skills/converge/target/release/converge \
  -s pr-OWNER-REPO-PR \
  --hook 'npx tsx ~/code/skills/pr-fitness/src/pr-progress-hook.ts OWNER/REPO PR' \
  -- npx tsx ~/code/skills/pr-fitness/src/cli.ts -q -c OWNER/REPO PR
```

Always pass `--hook` with pr-fitness to post progress comments on the PR.

## Compose

```
/timebox 90m /converge ... -- <fitness-cmd>
/loop 30m /converge ... -- <fitness-cmd>
```
