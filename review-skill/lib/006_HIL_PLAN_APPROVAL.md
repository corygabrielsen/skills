# HIL: Plan Approval

**Present plan to user BEFORE making edits.**

**If `--auto` mode:** Show themes and proposed fixes, then proceed directly to Address (skip AskUserQuestion).

## Do:
- Show themes and proposed fixes
- Use AskUserQuestion with clear options
- Wait for explicit approval

## Don't:
- Make edits before approval
- Skip showing the plan (even in `--auto` mode, always show the plan before proceeding)

## Options

```
AskUserQuestion(
  questions: [{
    question: "Approve plan to address these issues?",
    header: "Plan",
    options: [
      {label: "Approve", description: "Proceed to make edits"},
      {label: "Modify", description: "I'll provide different approach"},
      {label: "Abort", description: "Do not make changes"}
    ],
    multiSelect: false
  }]
)
```

## Handlers

**If user selects "Approve":** Proceed to Address phase.

**If user selects "Modify":**
1. Acknowledge selection and prompt: "Please describe what changes to the plan you'd like (adjust proposed fixes, change resolution types, etc.)."
2. End turn (stop responding and wait for user input).
3. When user provides input, update plan accordingly.
4. Show updated plan to user (same format as original Triage output).
5. Re-present Plan Approval options (repeat from step 1 until user selects Approve or Abort).

**If user selects "Abort":** End skill without changes.
