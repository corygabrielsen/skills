# HIL: Next Action

**Human decides what happens next after status report.**

**If `--auto` mode:** Automatically continue if more work exists, otherwise complete.

## Do:
- Present current state summary
- Offer clear next action options
- Wait for human decision
- Execute chosen path

## Don't:
- Assume continuation (unless `--auto`)
- Loop forever without checkpoint

## Options

```
AskUserQuestion(
  questions: [{
    question: "What's next?",
    header: "Next",
    options: [
      {label: "Continue", description: "Delegate remaining tasks"},
      {label: "Pause", description: "Save state, return control to me"},
      {label: "Add work", description: "I have more tasks to add"},
      {label: "Complete", description: "Mission accomplished, wrap up"}
    ],
    multiSelect: false
  }]
)
```

## Handlers

**If "Continue":**
1. Check for ready tasks (pending, empty blockedBy)
2. If ready tasks exist → preflight/EVALUATE
3. If all tasks blocked with no in_progress tasks → **deadlock detected**:
   - Circular dependencies (A blockedBy B, B blockedBy A) → HIL_ANOMALY (Classification: Blocking)
   - Stale blockers (blockedBy tasks that are ABORTED/completed) → offer to clear; if user accepts, clear and → preflight/EVALUATE; if user declines, re-present options
   - Otherwise, present AskUserQuestion: "Unblock manually" (user will explain) or "Abort blocked tasks" (mark all blocked as ABORTED). After resolution → preflight/EVALUATE or control/REPORT if nothing remains
4. If all tasks complete → proceed to "Complete" handler

**If "Pause":**
1. → control/HANDOFF
2. Report: "State saved. Resume with `/mission-control`."
3. End skill

**If "Add work":**
1. Prompt: "Describe the additional work."
2. End turn, wait for user input.
3. If user provides empty/unclear input or cancels ("nevermind") → re-present HIL_NEXT_ACTION options
4. Otherwise → setup/DECOMPOSE with new input; new tasks added to existing graph

**If "Complete":**
1. Verify all tasks are completed or ABORTED
2. If incomplete tasks remain, confirm: "These tasks are still open: [list]. Mark them ABORTED?"
   - If user says yes → mark listed tasks ABORTED, continue to step 3
   - If user says no → re-present HIL_NEXT_ACTION options (mission not complete)
3. Generate final summary
4. End skill

## Auto Mode Logic

```
if --auto:
    if ready_tasks exist (pending with empty blockedBy):
        → preflight/EVALUATE
    else if in_progress_tasks exist:
        → execution/MONITOR (covers tasks already running from previous batches)
    else if all_tasks completed:
        → generate summary, end skill
    else:
        → deadlock: all pending tasks blocked AND no in_progress tasks
        → no forward progress possible without intervention
        → exit auto mode, present options to human
```

**Auto mode exit is permanent:** Once `--auto` exits for human intervention (failure, NO-GO, or deadlock), it remains disabled for the rest of the session. Human must explicitly pass `--auto` on next `/mission-control` invocation to re-enable.
