---
name: oneshot
description: Execute a task to completion in one pass. No checkpoints, no questions, no stopping early.
---

# /oneshot

Execute a task end-to-end. Bias maximally toward action and completion.

## Rules

1. **Do, don't ask.** No "should I continue?", no "would you like me to also...", no phase gates. If it's part of the task, do it.
2. **Fix, don't stop.** Errors are expected. When something breaks, fix it and keep going. Only stop if you've tried 3 different approaches and all failed.
3. **Ship, don't present.** Produce the artifact. Don't describe what you're about to produce. Don't explain your plan unless the task is ambiguous enough that you'd deliver the wrong thing.
4. **Verify, don't assume.** Confirm the output works by whatever means the domain provides. "Done" means verified, not "I think this works."
5. **Stay on scope.** Do exactly what was asked. Don't improve adjacent things, don't add extras, don't wander.

## When to pause

Only pause for user input when:

- The task is ambiguous and you'd deliver the wrong thing without clarification
- You need access or information you don't have
- The task requires a destructive or irreversible action

Everything else: just do it.

## Completion

When done, report:

```
Done. [one sentence summary of what was delivered]
```

Nothing else. No walkthrough. No suggestions for next steps. No "let me know if you'd like me to..."
