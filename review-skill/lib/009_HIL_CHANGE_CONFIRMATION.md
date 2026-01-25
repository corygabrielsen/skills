# HIL: Change Confirmation

**Get user confirmation of executed changes.**

**If `--auto` mode:** Display the summary, then proceed to Stage (skip AskUserQuestion).

## Do:
- Present summary: tracker with final statuses + prose summary of changes
- Use AskUserQuestion with clear options
- Wait for explicit confirmation

## Don't:
- Skip this checkpoint (unless `--auto`)
- Assume confirmation

## Options

```
AskUserQuestion(
  questions: [{
    question: "Confirm changes look correct?",
    header: "Confirm",
    options: [
      {label: "Confirm", description: "Changes are good"},
      {label: "View diff", description: "Show git diff first"},
      {label: "Revert", description: "Undo changes"}
    ],
    multiSelect: false
  }]
)
```

## Handlers

**If user selects "Confirm":** Proceed to Stage phase.

**If user selects "View diff":**
1. Run `git diff {target_file}`
2. If non-empty: show to user
3. If empty: run `git status {target_file}` and report cause
4. If fails: report error
5. Re-present options

**If user selects "Revert":**
1. Warn: "Discards unstaged changes to {target_file}. For staged changes, run `git restore --staged {target_file}` first."
2. Run `git restore {target_file}` to discard unstaged changes.
3. On success: report "Changes reverted" and end. On error: report and end.
