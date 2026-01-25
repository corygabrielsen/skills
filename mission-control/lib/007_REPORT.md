# Report

**Present status and determine next action.**

## Do:
- Report status as table for scannability
- Show: task, status, blocked-by, owner
- Call out: ready, blocked, needs attention
- Determine next action based on mode and state

## Don't:
- Skip the status report
- Use prose when tables are clearer
- Continue without reporting current state

## Status Table Format

```markdown
## Mission Status

| Task | Status | Blocked By | Owner | Notes |
|------|--------|------------|-------|-------|
| T-001 | completed | --- | agent-1 | Verified |
| T-002 | completed | --- | agent-2 | Verified |
| T-003 | in_progress | --- | agent-3 | Running |
| T-004 | pending | T-003 | --- | Waiting |
| T-005 | ABORTED | --- | --- | [reason] |
```

## Status Values

- `pending`: Not started, may be blocked
- `in_progress`: Agent is working on it
- `completed`: Done and verified
- `ABORTED - [reason]`: Cancelled (always include reason in subject)

## Next Action Logic

```
--fg mode:
    if more ready tasks:
        → Return to Delegate (loop continues)
    else if all tasks completed:
        → Report final status, end skill
    else if tasks are blocked with no progress possible:
        → Report blockers, end skill (or prompt user)

--bg mode:
    → Report status
    → Return control to human
    → Human can resume with /mission-control
```

## Final Report Template (All Work Complete)

```markdown
## Mission Complete

| Task | Result |
|------|--------|
| T-001 | [outcome] |
| T-002 | [outcome] |

All {n} tasks completed and verified.
```
