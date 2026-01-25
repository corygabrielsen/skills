# Handoff

**Capture state for resumption after compaction or session end.**

Like NASA shift handoffs: outgoing controller briefs incoming controller on everything they need to continue.

## Triggers
- Context approaching compaction limit
- User selects "Pause" in HIL: Next Action
- Session ending
- Explicit user request

## Do:
- Capture all state that won't survive compaction
- Write to task descriptions and metadata (these persist)
- Ensure fresh agent can continue without asking
- Report handoff summary to user

## Don't:
- Rely on context memory (will be lost)
- Leave implicit state undocumented
- Assume current agent will continue

## State to Capture

| State | Where to Store | How |
|-------|----------------|-----|
| Task status | Task system | Already there via TaskList |
| In-flight agents | Task metadata | `metadata.agent_id` on each task |
| Decisions made | Task descriptions | Append to description |
| Open questions | New task | Create "QUESTION: ..." task |
| Mode flags | Task metadata | Store on a "meta" task |
| Iteration count | Task metadata | `metadata.iteration` |

## Handoff Format

Write to a designated task or create one:

```
TaskCreate(
  subject: "MISSION STATE - [timestamp]",
  description: """
  ## Mission Handoff

  ### Status Summary
  - Completed: 3 tasks
  - In Progress: 1 task (T-004, agent abc123)
  - Blocked: 2 tasks (waiting on T-004)
  - Pending: 0 tasks

  ### Key Decisions
  - Using JWT for auth (user preference from earlier)
  - Skipped rate limiting (out of scope per user)

  ### Open Questions
  - None currently

  ### Next Actions
  When resuming:
  1. Check if T-004 agent completed
  2. If complete, verify and unblock T-005, T-006
  3. Continue delegation cycle
  """,
  metadata: {
    type: "handoff",
    mode: "--fg",
    created_at: "[timestamp]"
  }
)
```

## Resumption Instructions

After handoff, inform user:

```markdown
## Handoff Complete

State captured in task system. To resume:

1. Run `/mission-control` (or `/mission-control --fg`)
2. Mission control will read TaskList and Bootstrap phase will recover state
3. Coordination continues from where we left off

**In-flight work:** T-004 (agent abc123) - check for completion
```

## Verification

Before ending:
- Run TaskList to confirm all state is captured
- Verify handoff task was created
- Confirm critical decisions are documented
