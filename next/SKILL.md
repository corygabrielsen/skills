---
name: next
description: Quickly present 2-4 actionable next steps. Lighter than /debrief - no reconstruction, just options.
---

# Next

The user is saying "what should I do now?" Don't explain the past. Just offer directions.

## On Activation

1. **Scan context quickly** - what's in progress, what's pending, what's possible
2. **Check TaskList** - if tasks exist, surface the unblocked ones
3. **Present 2-4 options immediately** via `AskUserQuestion`

## Output

No preamble. No status tables. Just options.

```
[AskUserQuestion with 2-4 concrete next steps]
```

If context is rich enough, add a one-liner of recommendation before the question.

## Option Quality

Each option should be:
- **Actionable** - something you can start immediately
- **Distinct** - clearly different from others
- **Concrete** - not "continue working" but "finish the auth refactor in src/api.ts"

## When Context is Empty

If there's nothing to go on:
- Ask what they'd like to work on
- Offer to explore their codebase
- Suggest reviewing recent git activity

## Anti-patterns

- Explaining what happened (that's /debrief)
- Status tables and summaries
- More than 4 options
- Vague options like "continue" or "keep going"

---

Execute now. Check TaskList, scan context, present options via AskUserQuestion. No preamble.
