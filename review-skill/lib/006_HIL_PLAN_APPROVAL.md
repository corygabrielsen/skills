# HIL: Plan Approval

**Present plan to user BEFORE making edits.**

**If `--auto` mode:** Show the plan, then proceed to Address (skip AskUserQuestion).

## Do:
- Show themes and proposed fixes (always)
- Use AskUserQuestion with clear options (unless `--auto`)
- Wait for explicit approval (unless `--auto`)

## Don't:
- Make edits before approval/showing plan
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
Users can change HOW issues are addressed, not WHETHER—all flagged issues require document changes (Core Philosophy). If user dismisses: remind of Core Philosophy, ask for alternative that includes a document change.

1. Prompt: "Describe changes to the plan."
2. Wait for input.
3. If empty/unclear, ask for clarification (max 2 rounds → Abort). Otherwise, revise.
4. Show updated plan.
5. Re-present options. If user selects Modify 3+ times, suggest aborting.

**If user selects "Abort":** End skill without changes.
