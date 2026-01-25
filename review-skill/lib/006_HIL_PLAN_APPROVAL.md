# HIL: Plan Approval

**Present plan to user BEFORE making edits.**

**If `--auto` mode:** Show the plan (themes and proposed fixes), then proceed directly to Address (skip AskUserQuestion but still display the plan).

## Do:
- Show themes and proposed fixes (always—even in `--auto` mode)
- Use AskUserQuestion with clear options (unless `--auto`)
- Wait for explicit approval (unless `--auto`)

## Don't:
- Make edits before approval (or before showing the plan in `--auto` mode)
- Skip showing the plan
- Re-run Synthesize/Triage when user modifies—just adjust fix proposals directly

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
Users can change HOW issues are addressed (different wording, alternative fixes), not WHETHER—all flagged issues still require document changes per Core Philosophy. Handle dismissal attempts: remind user of Core Philosophy, then ask for an alternative resolution that includes a document change.

1. Acknowledge selection and prompt: "Please describe what changes to the plan you'd like (adjust proposed fixes, change resolution types, etc.)."
2. Pause here and wait for user input (do not continue to Address phase yet).
3. When user provides input, revise the proposed fixes based on user feedback (don't re-run Synthesize/Triage phases; just adjust the fix proposals directly). If feedback is empty or unclear, ask for clarification; once clarified, continue with this step (revise fixes).
4. Show updated plan to user (same format as original Triage output).
5. Re-present Plan Approval options (repeat from step 1 until user selects Approve or Abort). Note: This Modify flow is only reachable in non-auto mode since `--auto` skips AskUserQuestion.

**If user selects "Abort":** End skill without changes.
