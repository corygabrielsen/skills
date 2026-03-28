---
name: timebox
description: Work autonomously for a fixed duration. Check the clock. Ship what fits.
args:
  - name: duration
    description: "Time limit (required): e.g. 15m, 30m, 1h, or a range like 20-30m, 1-2h"
  - name: task
    description: What to work on (optional — uses conversation context if omitted)
---

# /timebox

Work autonomously for a fixed duration. No questions. Check the system clock (`date`) at natural breakpoints. Don't start something you can't finish in the remaining time. When time's up, stop and report what you shipped.

If given a range (e.g. 20-30m), find a natural stopping point within that window.
