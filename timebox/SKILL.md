---
name: timebox
description: Work autonomously for a fixed duration. Check the clock. Ship what fits.
args:
  - name: duration
    description: "Time limit (required): e.g. 15m, 30m, 1h"
  - name: task
    description: Open-ended task description (optional — uses conversation context if omitted)
---

# /timebox

Work autonomously for a fixed duration. The clock decides scope.

## Invariants

- **Check the system clock** (`date`) at every natural breakpoint
- **Don't start work you can't finish** in the remaining time
- **No questions.** Decide and move.
- **Feature branch.** Commit and push as you go.
- **Ship independently valuable units** — each commit should stand alone

## The loop

```
record start time (date +%s)
while time remains:
  pick highest-impact next thing
  check clock — enough time? if not, wrap up
  do the thing
  build/typecheck
  commit and push
report what shipped
```

## When time's up

```
| Minute | What shipped |
|--------|-------------|
| 0-3    | Built X     |
| 3-7    | Built Y     |

Delivered: N items. PR: [link]
Remaining: [what didn't fit]
```

---

Parse args for duration (required). Record start time. Enter the loop.
