# Checkpoint

**Periodic status poll. "All stations, status check."**

Catches slow-developing problems before they become crises.

## Triggers
- After each Verify phase
- Periodically during long Monitor waits (--fg mode)
- On user request
- When context approaching compaction threshold

## Do:
- Poll TaskList for current state
- Identify: completed, in_progress, pending, blocked, stalled
- Surface anything needing attention
- Check for resource concerns (context size, time elapsed)

## Don't:
- Skip checkpoint when things "seem fine"
- Continue without surfacing stalled work

## Status Categories

| Category | Meaning | Action |
|----------|---------|--------|
| Completed | Done and verified | None |
| In Progress | Agent working | Monitor |
| Pending | Ready to launch | Delegate |
| Blocked | Waiting on dependencies | Check blockers |
| Stalled | In progress too long, no output | Investigate |
| Failed | Verification failed | → control/HIL_ANOMALY |

## Checkpoint Format

```markdown
## Checkpoint

| Task | Status | Notes |
|------|--------|-------|
| T-001 | completed | Verified |
| T-002 | in_progress | Agent actively working |
| T-003 | blocked | Waiting on T-002 |
| T-004 | in_progress | No recent output, monitoring |

### Attention Needed
- T-004: No visible progress, may need investigation

### Resource Status
- Context: ~40% capacity
- Tasks: 2/4 complete
```

## Stall Detection

No hard-coded timeouts. Agent capabilities evolve rapidly.

**Stall indicators (use judgment):**
- Agent has stopped producing output mid-task
- Task is blocking others with no visible progress
- User expresses concern
- Pattern differs from similar previous tasks

**Not stalled (patience required):**
- Agent actively working (tool calls visible)
- Complex research or multi-file edits in progress
- Agent explicitly indicated long-running work

**Relative timing:** If similar tasks completed in X time, 3-5X without progress is worth surfacing. When no baseline exists, wait until user engagement or visible output ceases.

When uncertain, surface to user: "T-004 has been running for [duration]. Continue waiting?"

## After Checkpoint

```
if stalled tasks exist:
    → control/HIL_ANOMALY
if context near limit:
    → control/HANDOFF
else:
    → control/REPORT
```
