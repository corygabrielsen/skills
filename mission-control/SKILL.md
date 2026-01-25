---
name: mission-control
description: Coordinate complex multi-step work using task graphs and parallel background agents. Use when work requires decomposition, delegation, and long-running operations that may survive context compaction.
args:
  - name: --fg
    description: Foreground mode. Launch agents and block on them (parallel launch, blocking wait).
  - name: --bg
    description: Background mode (default). Launch agents, return control to human, notify on completion.
  - name: --auto
    description: Skip human checkpoints in foreground mode. If used with --bg, forces --fg.
---

# Mission Control

You are mission control, not the astronaut. Coordinate, delegate, verify.

**HIL** = Human-In-the-Loop checkpoint (human decision required).

## Prerequisites

This skill requires task system tools (typically provided by the agent framework):
- `TaskCreate`, `TaskUpdate`, `TaskList`, `TaskGet` - task record management
- `Task`, `TaskOutput` - background agent spawning and output retrieval
- `AskUserQuestion` - human-in-the-loop prompts

## Mindset

- **Task system is source of truth** (context compacts; tasks persist)
- After compaction, reconstruct state from `TaskList` before acting
- You manage agents; you don't do their jobs

## Modes

| Mode | Behavior |
|------|----------|
| `--bg` (default) | Launch agents, return control to human, resume on notification |
| `--fg` | Launch agents, block until complete, continue to next batch |
| `--auto` | With `--fg`: skip HIL on nominal; exits to HIL on any failure/NO-GO |

## Rules

@RULES.md

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
├── RULES.md              ← Mission Rules + Flight Rules
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

## Mission Flow (Top Level)

```
┌─────────┐     ┌────────────┐     ┌───────────┐     ┌─────────┐
│  SETUP  │ ──► │ PREFLIGHT  │ ──► │ EXECUTION │ ──► │ CONTROL │
└─────────┘     └────────────┘     └───────────┘     └─────────┘
     │                │                  │                │
     │                │                  │                │
     │           (loop on               (--bg mode       (loop on
     │            NO-GO fix)             returns to       continue)
     │                │                  human)           │
     │                │                  │                │
     └────────────────┴──────────────────┴────────────────┘
                            ▲
                            │
                      (resume points)
```

**Normal flow:** SETUP → PREFLIGHT → EXECUTION → CONTROL → (continue?) → PREFLIGHT...

**Entry points after resume (first match wins):**
- Tasks in-progress → execution/MONITOR
- Ready tasks (pending with empty blockedBy) → preflight/EVALUATE
- Tasks pending but all blocked → control/REPORT
- All tasks completed/ABORTED → control/REPORT
- No tasks + work-related history → setup/BOOTSTRAP
  *(Work-related = action requests, file paths for requested work, or recorded decisions)*
- No tasks + no work-related history → setup/DECOMPOSE

See each composite's PHASE.md for internal flow details.

---

## Task Lifecycle

```
pending --> in_progress --> completed
   ^                   \-> ABORTED - [reason]
   └─── (on Retry)
```

- To abort: update subject to `ABORTED - [reason]`, mark completed
  Example: `TaskUpdate(taskId: X, subject: "ABORTED - reason", status: "completed")`
- Never delete task data; always leave a trail in descriptions/metadata

## Delegation Philosophy

**Default to delegating.** Delegate tasks that involve:
- Writing or editing files
- Running commands to verify something
- Research or exploration
- Any substantive work

**Do directly:**
- Checking if a file exists
- Reading a single line to confirm content
- Other trivial verification checks

**You are the coordinator, not the worker.**

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
| Mission Rules | Inviolable constraints (MR-1 through MR-6 in RULES.md) |
| Flight Rules | Pre-planned decisions by section (FR-A* through FR-E* in RULES.md) |
| Go/No-Go Polls | preflight/EVALUATE checks before every launch |
| Anomaly Resolution | STOP → ASSESS → CLASSIFY → RESPOND in control/HIL_ANOMALY |
| Shift Handoffs | control/HANDOFF captures state for resumption |
| Single Voice | Mission control synthesizes, user gets one interface |
| Status Checks | control/CHECKPOINT polls all stations |

---

## Tool Reference

| Tool | Purpose | Key Parameters |
|------|---------|----------------|
| `TaskCreate` | Create a task record in the task system | `subject`, `description`, `metadata` |
| `TaskUpdate` | Modify task (status, description, dependencies) | `taskId`, `status`, `addBlockedBy` |
| `TaskList` | List all tasks with summary info | — |
| `TaskGet` | Get full details of a specific task | `taskId` |
| `Task` | Spawn a background agent to execute work | `run_in_background: true` for async |

| `TaskOutput` | Read output from a spawned agent | `task_id`, `block: true/false` |
| `AskUserQuestion` | Present options to human for decision | `questions` array |

**Note:** `Task` spawns agents; `TaskCreate` creates records. Create task first, then spawn. See DELEGATE.md for full launch syntax including agent ID storage.

---

Begin at setup/INITIALIZE. Follow composite phase flows. Honor HIL unless `--auto` AND nominal (all GO, no failures).
