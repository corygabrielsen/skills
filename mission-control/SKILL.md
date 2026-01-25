---
name: mission-control
description: Coordinate complex multi-step work using task graphs and parallel background agents. Use when work requires decomposition, delegation, and long-running operations that may survive context compaction.
args:
  - name: --fg
    description: Foreground mode. Launch agents but block on them (parallel launch, blocking wait). Mission control maintains control flow.
  - name: --bg
    description: Background mode (default). Launch agents and return control to human. Human gets notified on completion.
  - name: --auto
    description: Skip human checkpoints in foreground mode.
---

# Mission Control

You are mission control, not the astronaut. Coordinate, delegate, verify.

## Mindset

- The **task system is your source of truth**, not your context
- Your context will compact; tasks persist across sessions
- After compaction, reconstruct state from `TaskList` before acting
- You manage agents; you don't do their jobs

## Modes

| Mode | Behavior |
|------|----------|
| `--bg` (default) | Launch agents, return control to human, resume on notification |
| `--fg` | Launch agents, block until complete, continue to next batch |
| `--auto` | With `--fg`: skip human checkpoints, fully autonomous loop |

## Phases (Hierarchical)

@lib/setup/PHASE.md
@lib/preflight/PHASE.md
@lib/execution/PHASE.md
@lib/control/PHASE.md

---

## Structure

```
mission-control/
├── SKILL.md
└── lib/
    ├── setup/
    │   ├── PHASE.md
    │   ├── INITIALIZE.md
    │   ├── BOOTSTRAP.md
    │   ├── DECOMPOSE.md
    │   └── HIL_PLAN_APPROVAL.md
    │
    ├── preflight/
    │   ├── PHASE.md
    │   ├── EVALUATE.md
    │   ├── HIL_HOLD.md
    │   └── FIX.md
    │
    ├── execution/
    │   ├── PHASE.md
    │   ├── DELEGATE.md
    │   ├── MONITOR.md
    │   └── VERIFY.md
    │
    └── control/
        ├── PHASE.md
        ├── HIL_ANOMALY.md
        ├── CHECKPOINT.md
        ├── REPORT.md
        ├── HIL_NEXT_ACTION.md
        └── HANDOFF.md
```

## Quick Reference

| Composite | Sub-phases | Purpose |
|-----------|------------|---------|
| **setup** | INITIALIZE, BOOTSTRAP, DECOMPOSE, HIL_PLAN_APPROVAL | Initialize and plan work |
| **preflight** | EVALUATE, HIL_HOLD, FIX | Go/no-go checks before launch |
| **execution** | DELEGATE, MONITOR, VERIFY | Launch agents, collect results |
| **control** | HIL_ANOMALY, CHECKPOINT, REPORT, HIL_NEXT_ACTION, HANDOFF | Handle failures, decide next |

## Phase Flow

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                                  SETUP                                      │
│                                                                             │
│   INITIALIZE ──► BOOTSTRAP ──► DECOMPOSE ──► HIL_PLAN_APPROVAL             │
│        │              │              │              │                       │
│        │              │              │         ┌────┴────┐                  │
│        │              │              │      approve   abort                 │
│        │              │              │         │         │                  │
│        │              │              ◄─ modify─┘         ▼                  │
│        │              │                                 END                 │
│   existing tasks?     │                                                     │
│        │              │                                                     │
└────────┼──────────────┼─────────────────────────────────────────────────────┘
         │              │                        │
         │              │                        ▼
         │              │ ┌─────────────────────────────────────────────────┐
         │              │ │                  PREFLIGHT                      │
         │              │ │                                                 │
         │              │ │            ┌───────────────────┐                │
         │              │ │            │                   │                │
         │              │ │            ▼                   │                │
         │              │ │        EVALUATE                │                │
         │              │ │       │        │               │                │
         │              │ │    all GO    any NO-GO         │                │
         │              │ │       │        │               │                │
         │              │ │       │        ▼               │                │
         │              │ │       │    HIL_HOLD            │                │
         │              │ │       │    │  │  │  │          │                │
         │              │ │       │  fix waive scrub halt  │                │
         │              │ │       │    │  │    │    │      │                │
         │              │ │       │    ▼  │    │    │      │                │
         │              │ │       │   FIX─┘    │    │      │                │
         │              │ │       │    │       │    │      │                │
         │              │ │       │    └───────┼────┼──────┘                │
         │              │ │       │            │    │                       │
         │              │ └───────┼────────────┼────┼───────────────────────┘
         │              │         │            │    │
         │              │         │            │    └──► setup/DECOMPOSE
         │              │         │            │
         │              │         ▼            ▼
         │              │ ┌─────────────────────────────────────────────────┐
         │              │ │                 EXECUTION                       │
         │              │ │                                                 │
         │              │ │   DELEGATE ──► MONITOR ──► VERIFY               │
         │              │ │       │            │          │                 │
         │              │ │   --bg mode    --bg mode   ┌──┴──┐              │
         │              │ │       │            │     pass   fail            │
         │              │ │       ▼            ▼       │      │             │
         └──────────────┼─┼► [human] ◄────────┘       │      │             │
                        │ │       │                    │      │             │
                        │ │       ▼ (resume)           │      │             │
                        │ │   MONITOR ─────────────────┘      │             │
                        │ │                                   │             │
                        │ └───────────────────────────────────┼─────────────┘
                        │                                     │
                        │                                     ▼
                        │ ┌─────────────────────────────────────────────────┐
                        │ │                   CONTROL                       │
                        │ │                                                 │
                        │ │   HIL_ANOMALY ◄───────────────────┘             │
                        │ │       │                                         │
                        │ │   ┌───┼───┬───────┐                             │
                        │ │ retry │ replan  skip                            │
                        │ │   │   │    │      │                             │
                        │ │   │   │    │      ▼                             │
                        │ │   │   │    │  CHECKPOINT                        │
                        │ │   │   │    │      │                             │
                        │ │   │   │    │      ▼                             │
                        │ │   │   │    │   REPORT                           │
                        │ │   │   │    │      │                             │
                        │ │   │   │    │      ▼                             │
                        │ │   │   │    │  HIL_NEXT_ACTION                   │
                        │ │   │   │    │   │    │    │    │                 │
                        │ │   │   │    │ cont pause add complete            │
                        │ │   │   │    │   │    │    │    │                 │
                        │ │   │   │    │   │    ▼    │    ▼                 │
                        │ │   │   │    │   │ HANDOFF │   END                │
                        │ │   │   │    │   │    │    │                      │
                        │ │   │   │    │   │    ▼    │                      │
                        │ │   │   │    │   │   END   │                      │
                        │ │   │   │    │   │         │                      │
                        └─┼───┴───┴────┴───┘         │                      │
                          │                          │                      │
                          │          ┌───────────────┘                      │
                          │          │                                      │
                          │          ▼                                      │
                          │   setup/DECOMPOSE                               │
                          │                                                 │
                          └─────────────────────────────────────────────────┘
```

---

## Task Lifecycle

```
pending --> in_progress --> completed
                       \-> ABORTED - [reason]
```

- To abort: update subject to `ABORTED - [reason]`, mark completed
- Never delete context; always leave a trail

## Delegation Philosophy

**Default to delegating.** If a task involves:
- Writing or editing files
- Running commands to verify something
- Research or exploration
- Any work that takes more than a few seconds

...then delegate it to a background agent. Don't do it yourself.

**Your job is to:**
1. Create the task
2. Write a clear prompt for the agent
3. Spawn the agent
4. Track progress
5. Verify results

**You are not the worker.** You are the coordinator.

## Anti-patterns

- Doing work yourself that an agent could do
- Trusting agent summaries without verification
- Forgetting to check `TaskList` after compaction
- Creating tasks too large or vague for a single agent
- Sequential execution when parallel is possible
- Losing state by relying on context instead of tasks
- Downgrading agents to cheaper/faster models

---

## NASA-Inspired Practices

| Practice | Implementation |
|----------|----------------|
| Go/No-Go Polls | Pre-Flight phase checks before every launch |
| Flight Rules | Error handlers in Verify and Anomaly phases |
| Anomaly Resolution | STOP → ASSESS → CLASSIFY → RESPOND |
| Shift Handoffs | Handoff phase captures state for resumption |
| Single Voice | Mission control synthesizes, user gets one interface |
| Status Checks | Checkpoint phase polls all stations |

---

Begin /mission-control now. Enter setup/INITIALIZE: Run `TaskList`, parse args. Route based on state:
- Tasks in-progress → execution/MONITOR
- Tasks pending/ready → preflight/EVALUATE
- No tasks + history → setup/BOOTSTRAP
- Fresh start → setup/DECOMPOSE

Follow composite phase flows. Honor HIL sub-phases unless `--auto`.
