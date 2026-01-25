# Preflight: HIL Hold

**Human decides how to handle NO-GO conditions.**

Like NASA's launch hold: stop, assess, decide whether to scrub, hold, or resolve.

## Entry
- One or more tasks evaluated as NO-GO in EVALUATE phase
- GO tasks from the same evaluation batch remain ready---they wait while NO-GOs are resolved

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
→ preflight/FIX

**If "Waive":**
1. Record waiver: `metadata.waived: true, metadata.waiver_reason: "..."`
2. Log warning: "Proceeding with waived NO-GOs. Increased failure risk."
3. → execution/DELEGATE

**If "Scrub":**
1. Mark NO-GO tasks as `ABORTED - Scrubbed at pre-flight`
2. If GO tasks remain → execution/DELEGATE
3. If no GO tasks remain → control/REPORT

**If "Halt":**
→ setup/DECOMPOSE
