# Pre-Flight: HIL Hold

**Human decides how to handle NO-GO conditions.**

Like NASA's launch hold: stop, assess, decide whether to scrub, hold, or resolve.

## Entry
- One or more tasks evaluated as NO-GO in EVALUATE phase

## Do:
- Present NO-GO summary with reasons
- Offer clear resolution options
- Wait for human decision

## Don't:
- Auto-resolve non-trivial issues
- Proceed without human decision
- Assume scrub is the only option

## Options

```
AskUserQuestion(
  questions: [{
    question: "How should we handle these NO-GO conditions?",
    header: "Hold",
    options: [
      {label: "Fix", description: "Attempt to resolve NO-GOs"},
      {label: "Waive", description: "Proceed despite NO-GOs (risky)"},
      {label: "Scrub", description: "Abort these tasks, continue with GOs only"},
      {label: "Halt", description: "Stop mission, return to planning"}
    ],
    multiSelect: false
  }]
)
```

## Handlers

**If "Fix":**
- → proceed to preflight/FIX

**If "Waive":**
- Record waiver in task metadata: `metadata.waived: true, metadata.waiver_reason: "..."`
- Log warning: "Proceeding with waived NO-GOs. Increased failure risk."
- → exit preflight, proceed to execution/DELEGATE

**If "Scrub":**
- Mark NO-GO tasks as `ABORTED - Scrubbed at pre-flight`
- If GO tasks remain → exit preflight, proceed to execution/DELEGATE
- If no GO tasks remain → proceed to control/REPORT

**If "Halt":**
- → return to setup/DECOMPOSE for replanning
