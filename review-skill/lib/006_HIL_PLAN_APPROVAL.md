# HIL: Plan Approval

**Present plan to user BEFORE making edits.**

**If `--auto` mode:** Show the plan (themes and proposed fixes), then proceed directly to Address (no AskUserQuestion).

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

1. Prompt: "Describe changes to the plan (adjust fixes, change resolution types)."
2. Wait for user input.
3. If input empty/unclear, ask for clarification (max 2 rounds, then treat as Abort). Otherwise, revise fix proposals.
4. Show updated plan.
5. Re-present options (loop until Approve or Abort).

**If user selects "Abort":** End skill without changes.
