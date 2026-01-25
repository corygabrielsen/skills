# HIL: Anomaly

**Structured failure handling with human decision.**

Triggered when: agent fails, unexpected output, verification fails, task blocked unexpectedly, or task appears stalled.

NASA's Anomaly Resolution: STOP → ASSESS → CLASSIFY → RESPOND. Don't make it worse.

## Do:
- STOP: Pause affected task, don't retry blindly
- ASSESS: Gather information about what happened
- CLASSIFY: Determine failure type
- Present classification and options to human
- Execute human's chosen response

## Don't:
- Immediately retry (might repeat failure)
- Ignore and continue (cascading failures)
- Panic-fix without understanding
- Make autonomous decisions on non-trivial failures

## Classification

| Type | Meaning | Suggested Response |
|------|---------|-------------------|
| Transient | Flaky, might work on retry | Suggest retry to human |
| Systematic | Approach is wrong | Replan task |
| Blocking | Need info we don't have | Escalate, ask user |
| Stalled | No progress, unclear if stuck | Wait longer or investigate |
| Fatal | Cannot recover | Abort task with explanation |

**Note:** All responses require human approval. "Suggest retry" means present Retry option; don't auto-retry (see FR-B001).

## Assessment Format

```markdown
## Anomaly Report

**Task:** T-003 - Implement auth middleware
**Phase:** Verify (agent completed, verification failed)

### What Happened
Agent reported success, but tests fail with: `AuthError: token undefined`

### Assessment
- Agent implemented middleware but didn't wire it to request context
- Tests expect `req.user` to be populated after auth

### Classification: Systematic
Approach was incomplete. Agent needs clearer requirements about request context integration.

### Recommended Response
Create follow-up task with explicit wiring requirements.
```

## Options

```
AskUserQuestion(
  questions: [{
    question: "How should we handle this anomaly?",
    header: "Anomaly",
    options: [
      {label: "Retry", description: "Try the same task again"},
      {label: "Replan", description: "Create new task with different approach"},
      {label: "Skip", description: "Mark task ABORTED, continue with others"},
      {label: "Halt", description: "Stop mission, investigate manually"}
    ],
    multiSelect: false
  }]
)
```

**Fallback:** If `AskUserQuestion` is unavailable, present options as a numbered list and ask user to reply with their choice.

## Handlers

**If "Retry":**
1. Reset task to `pending` (if task was `in_progress`, the previous agent's work is abandoned—it may complete but results are ignored)
2. → preflight/EVALUATE

**If "Replan":**
1. Mark original task `ABORTED - Replanning`
2. Prompt user: "Describe the new approach, or I'll propose one."
3. End turn, wait for user input:
   - If user provides approach → create new task with user's approach
   - If user says "propose" or similar → mission control proposes approach, asks user to confirm
   - If user cancels ("nevermind") → re-present Anomaly options
4. → preflight/EVALUATE

**If "Skip":**
1. Mark task `ABORTED - Skipped after anomaly`
2. Check if downstream tasks are now blocked
3. → control/CHECKPOINT

**If "Halt":**
1. Keep task `in_progress` (preserve state)
2. → control/HANDOFF
3. End skill

## Multiple Failures

When multiple tasks fail verification in the same batch:

1. Present all failures in a single Anomaly Report (list each task with its classification)
2. Offer batch actions: "Same action for all" or "Handle individually"
3. If "Same action for all": apply chosen action to all failed tasks, then proceed
4. If "Handle individually": iterate through each failed task, presenting options one at a time
5. After all failures handled → next phase based on last action taken
