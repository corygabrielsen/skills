---
name: mission-control
description: Coordinate complex multi-step work using task graphs and parallel background agents. Use when work requires decomposition, delegation, and long-running operations that may survive context compaction.
args:
  - name: --fg
    description: Foreground mode. Launch agents but block on them (parallel launch, blocking wait). Mission control maintains control flow.
  - name: --bg
    description: Background mode (default). Launch agents and return control to human. Human gets notified on completion.
  - name: --auto
    description: Skip human checkpoints in foreground mode. If used with --bg, forces --fg.
---

# Mission Control

You are mission control, not the astronaut. Coordinate, delegate, verify.

**HIL** = Human-In-the-Loop: sub-phases where mission control pauses for human decision.

## Prerequisites

This skill requires Claude Code's task system tools:
- `TaskCreate`, `TaskUpdate`, `TaskList`, `TaskGet` - task record management
- `Task`, `TaskOutput` - background agent spawning and output retrieval
- `AskUserQuestion` - human-in-the-loop prompts

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

**Entry points after resume:**
- Tasks in-progress → execution/MONITOR
- Ready tasks (pending with empty blockedBy) → preflight/EVALUATE
- Tasks pending but all blocked → control/REPORT
- All tasks completed/ABORTED → control/REPORT
- No tasks + work-related history → setup/BOOTSTRAP (see INITIALIZE.md for "history" definition)
- Fresh start → setup/DECOMPOSE

See each composite's PHASE.md for internal flow details.

---

## Task Lifecycle

```
pending --> in_progress --> completed
                       \-> ABORTED - [reason]
```

- To abort: update subject to `ABORTED - [reason]`, mark completed
- Never delete task data; always leave a trail in descriptions/metadata

## Delegation Philosophy

**Default to delegating.** If a task involves:
- Writing or editing files
- Running commands to verify something
- Research or exploration
- Any substantive work (not trivial verification like checking a file exists)

...then delegate it to a background agent. Don't do it yourself. Brief verification checks (confirming an artifact exists, reading a single line) can be done directly by mission control.

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

**Note:** `Task` (spawns agent) is distinct from `TaskCreate` (creates task record). Always create the task first, then spawn an agent to execute it.

**TaskOutput blocking:** Use `block: true` to wait for completion (--fg mode). Use `block: false` or omit to poll without waiting (--bg mode status checks).

---

Begin /mission-control now. Enter setup/INITIALIZE for state detection and routing.

Follow composite phase flows. Honor HIL sub-phases unless `--auto` AND nominal (see FR-E003 for auto-mode boundaries on failures).
