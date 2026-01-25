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
| Failed | Verification failed | → HIL: Anomaly |

## Checkpoint Format

```markdown
## Checkpoint

| Task | Status | Duration | Notes |
|------|--------|----------|-------|
| T-001 | completed | 2m | Verified |
| T-002 | in_progress | 5m | Agent running |
| T-003 | blocked | --- | Waiting on T-002 |
| T-004 | stalled | 12m | No output, may need intervention |

### Attention Needed
- T-004 appears stalled (12m with no progress)

### Resource Status
- Context: ~40% capacity
- Tasks: 2/4 complete
```

## Stall Detection

```
A task is stalled if:
    - Status is in_progress
    - No output received in threshold time:
        - Simple tasks: 5 minutes
        - Complex tasks: 15 minutes
        - Research tasks: 20 minutes
```

## After Checkpoint

```
if stalled tasks exist:
    → Trigger HIL: Anomaly for each
if context near limit:
    → Trigger Handoff
else:
    → Proceed to Report
```
