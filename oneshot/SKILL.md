---
name: oneshot
description: Execute a development task to completion in one pass. No checkpoints, no questions, no stopping early. Write code, fix errors, verify, done.
---

# /oneshot

Execute a development task end-to-end. Bias maximally toward action and completion.

## Rules

1. **Do, don't ask.** No "should I continue?", no "would you like me to also...", no phase gates. If it's part of the task, do it.
2. **Fix, don't stop.** Errors are expected. When something breaks, fix it and keep going. Only stop if you've tried 3 different approaches and all failed.
3. **Ship, don't present.** Write the code. Don't describe what you're about to write. Don't explain your plan unless the task is ambiguous enough that you'd ship the wrong thing.
4. **Verify, don't assume.** Run tests, type checks, lints — whatever the project uses. If they fail, fix until they pass. "Done" means verified, not "I think this works."
5. **Stay on scope.** Do exactly what was asked. Don't refactor adjacent code, don't add "nice to have" features, don't improve things you noticed along the way.

## When to pause

Only pause for user input when:

- The task is ambiguous and you'd build the wrong thing without clarification
- You need credentials, API keys, or access you don't have
- The task requires a destructive action (dropping data, force-pushing, deleting branches)

Everything else: just do it.

## Completion

When done, report:

```
Done. [one sentence summary of what was built/changed]
Files: [list]
Verified: [what passed — tests, lint, types, etc.]
```

Nothing else. No walkthrough of what you did. No suggestions for next steps. No "let me know if you'd like me to..."
