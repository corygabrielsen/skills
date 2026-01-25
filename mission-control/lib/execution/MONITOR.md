# Monitor

**Track agent progress and collect results.**

## Do:
- Check status of in_progress tasks via `TaskOutput`
- Handle completion notifications
- Record results for verification
- Update task status when agents complete

## Don't:
- Trust completion notifications blindly (verification happens next)
- Block on tasks in --bg mode (return control to human)
- Forget to check TaskList after context compaction

## Mode-Specific Behavior

```
--fg (foreground):
    Blocking already happened in Delegate phase.
    This phase is a pass-through; proceed immediately to VERIFY.
    → execution/VERIFY

--bg (background):
    This phase triggers when:
    - User resumes conversation
    - Task completion notification arrives

    Actions:
    1. Run TaskList to see current state
    2. Check TaskOutput(task_id, block: false) for any in_progress tasks (non-blocking poll)
    3. Collect results for completed tasks

    if some tasks completed, some still in_progress:
        Verify completed tasks first (→ execution/VERIFY)
        Remaining in_progress tasks continue monitoring on next cycle
        (via VERIFY → CHECKPOINT → REPORT → resume → MONITOR)

    if all in_progress tasks completed:
        → execution/VERIFY

    if no tasks completed yet:
        Report status and return control to human
```

## Polling Pattern

When checking task status (especially after compaction or notification):

```bash
# Check specific task output
TaskOutput(task_id: "<id>")

# Or poll all in_progress tasks from TaskList
```

## Handling Lost Notifications

Background task notifications are unreliable (~50% lost). If user says "tasks are done":
1. Trust them
2. Poll TaskOutput for all in_progress tasks immediately
3. Proceed with results
