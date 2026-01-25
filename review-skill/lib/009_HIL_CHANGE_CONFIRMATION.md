# HIL: Change Confirmation

**Get user confirmation of executed changes.**

**If `--auto` mode:** Display the summary (tracker with final statuses + prose summary), then proceed directly to Stage (skip AskUserQuestion).

## Do:
- Present summary: issue tracker showing final statuses, plus brief prose summary of key changes
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
1. Run `git diff {target_file}` to show unstaged changes (Edit tool produces unstaged changes).
2. If diff output is non-empty: show it to user.
3. If diff output is empty: run `git status {target_file}` to distinguish cause:
   - File is tracked with no unstaged changes: report "No unstaged changes to show."
   - File is untracked: report "File is untracked (not yet committed)."
4. If `git diff` fails unexpectedly: report error and re-present options.
5. After handling any case, re-present confirmation options.

**If user selects "Revert":**
1. Warn user: "This will discard unstaged changes to {target_file}. Staged changes are not affected—use `git restore --staged {target_file}` first if needed."
2. Run `git checkout -- {target_file}` to restore the file.
3. Handle edge cases:
   - Success: report "Changes reverted." and end skill.
   - File never committed (git errors "pathspec did not match"): report this error and end skill.
   - Other git error: report the error and end skill.
