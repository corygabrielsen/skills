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

## Phases

@lib/001_INITIALIZE.md
@lib/002_BOOTSTRAP.md
@lib/003_DECOMPOSE.md
@lib/004_HIL_PLAN_APPROVAL.md
@lib/005_PRE_FLIGHT.md
@lib/006_DELEGATE.md
@lib/007_MONITOR.md
@lib/008_VERIFY.md
@lib/009_HIL_ANOMALY.md
@lib/010_CHECKPOINT.md
@lib/011_REPORT.md
@lib/012_HIL_NEXT_ACTION.md
@lib/013_HANDOFF.md

---

## Quick Reference

| Phase | Type | Purpose |
|-------|------|---------|
| Initialize | Auto | Run TaskList, parse args for mode |
| Bootstrap | Auto | Mine conversation for work (if TaskList empty + history exists) |
| Decompose | Auto | Break request into discrete tasks with dependencies |
| HIL: Plan Approval | HIL | Human approves task breakdown (skipped with `--auto`) |
| Pre-Flight | Auto | Go/no-go checks before launching |
| Delegate | Auto | Launch agents for ready tasks |
| Monitor | Auto | Track progress, collect results |
| Verify | Auto | Validate completed work matches expectations |
| HIL: Anomaly | HIL | Structured failure handling with human decision |
| Checkpoint | Auto | Periodic status poll |
| Report | Auto | Status table |
| HIL: Next Action | HIL | Human decides continuation (skipped with `--auto`) |
| Handoff | Auto | Capture state for resumption |

## Phase Flow

```
INITIALIZE ──► BOOTSTRAP (if needed) ──► DECOMPOSE
                                              │
                                              ▼
                              ┌──── HIL: PLAN APPROVAL
                              │              │
                              │              ▼
                              │        PRE-FLIGHT ◄───── fix NO-GOs
                              │              │
                              │              ▼
                              │          DELEGATE
                              │              │
                              │              ▼
                              │          MONITOR
                              │              │
                              │              ▼
                              │          VERIFY ────► HIL: ANOMALY
                              │              │              │
                              │              ▼              │
                              │        CHECKPOINT ◄────────┘
                              │              │
                              │              ▼
                              │          REPORT
                              │              │
                              │              ▼
                              │     HIL: NEXT ACTION
                              │        │    │    │
                              │   continue  │   complete
                              │        │   pause   │
                              └────────┘    │      ▼
                                            ▼    END
                                        HANDOFF
                                            │
                                            ▼
                                          END
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

Begin /mission-control now. Run `TaskList` to check state. Parse args for `--fg`/`--bg`/`--auto`. Follow phase flow: if tasks exist, resume from appropriate phase (Monitor if in-progress, Verify if waiting); if empty with history, Bootstrap; otherwise await Decompose of user request. Honor HIL checkpoints unless `--auto`.
