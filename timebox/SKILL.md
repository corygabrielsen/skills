---
name: timebox
description: Work autonomously for a fixed duration. Check the clock. Ship what fits.
args:
  - name: duration
    description: "Time limit: e.g. 15m, 30m, 1h (default: 20m)"
  - name: task
    description: Open-ended task description (optional — uses conversation context if omitted)
---

# /timebox

Work autonomously for a fixed duration. The user is away. Check the system clock at checkpoints. Ship what fits. Stop when time's up.

## Why This Works

Parkinson's law: work expands to fill the time available. A timebox inverts this — you have N minutes, ship what you can. No deliberation on scope. No asking permission. The clock decides what's in and what's out.

The user steps away (shower, coffee, walk). You work. They come back to a summary of what shipped.

## On Activation

1. **Record start time** — `date +%s` (not wall clock — immune to timezone confusion)
2. **Plan** — 30 seconds max. Prioritize by impact. Don't plan more than fits.
3. **Work** — Build, commit, push. Check the clock at natural breakpoints.
4. **Checkpoint** — Every ~5 minutes, `date +%H:%M`. Are you on pace? Adjust scope.
5. **Wrap** — When time is up (or the last unit of work completes within the window), stop. Commit everything. Push.
6. **Report** — Minute-by-minute log of what shipped. Total time. What's left.

## Clock Discipline

```bash
# At start
START=$(date +%s)

# At checkpoints
NOW=$(date +%s); ELAPSED=$(( (NOW - START) / 60 )); echo "${ELAPSED}m elapsed"
```

Check the clock BEFORE starting a new unit of work. If there isn't enough time to finish it, don't start it — wrap up instead.

## Work Style

- **No questions.** The user is away. Make decisions and move.
- **Commit early, commit often.** Small commits that each build.
- **Build and typecheck between units.** Don't accumulate errors.
- **Feature branch.** Never commit to master.
- **Push at checkpoints.** Work is only real when it's pushed.

## Scope Management

If the task is open-ended ("improve X"), prioritize:

1. Highest-impact, lowest-effort first
2. Each unit of work should be independently valuable (shippable alone)
3. Don't start something you can't finish in the remaining time
4. Refactoring and cleanup go last (after new functionality)

## Report Format

When time is up:

```
## Timebox: [duration]

| Minute | What shipped |
|--------|-------------|
| 0-3    | Built X |
| 3-7    | Built Y |
| 7-9    | Refactored Z |

**Delivered:** [count] items. PR: [link]
**Remaining:** [what didn't fit]
```

## Anti-patterns

- **Planning for 10 minutes** — Plan ≤ 30 seconds. The clock is running.
- **Asking the user** — They're gone. Decide and go.
- **Starting something too big** — Check the clock first.
- **Perfecting before shipping** — Good enough and pushed beats perfect and local.
- **Forgetting to check the clock** — Set mental alarms at 25%, 50%, 75%.

---

Parse args for duration (default 20m). Record the start time. Start working. Check the clock at every natural breakpoint. Stop when time's up. Report what shipped.
