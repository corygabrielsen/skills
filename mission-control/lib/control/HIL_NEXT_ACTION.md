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
2. If ready tasks exist → Return to Pre-Flight
3. If all tasks blocked → Report blockers, re-present options
4. If all tasks complete → Proceed to "Complete" handler

**If "Pause":**
1. Trigger Handoff phase (capture state)
2. Report: "State saved. Resume with `/mission-control`."
3. End skill, return control to human

**If "Add work":**
1. Prompt: "Describe the additional work."
2. End turn, wait for user input.
3. Return to Decompose phase with new input
4. New tasks get added to existing graph

**If "Complete":**
1. Verify all tasks are completed or ABORTED
2. If incomplete tasks remain, confirm: "These tasks are still open: [list]. Mark them ABORTED?"
3. Generate final summary
4. End skill

## Auto Mode Logic

```
if --auto:
    if ready_tasks exist:
        → Continue (return to Pre-Flight)
    else if in_progress_tasks exist:
        → Continue monitoring (return to Monitor)
    else if all_tasks completed:
        → Complete (generate summary, end)
    else:
        → All tasks blocked with no progress possible
        → Exit auto mode, present options to human
```
