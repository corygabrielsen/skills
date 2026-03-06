---
name: postmortem
description: Write a structured postmortem for an incident. Thorough root cause analysis, not a dismissive summary.
args:
  - name: incident
    description: Brief description of what happened (or omit to analyze the most recent failure in context)
---

# Postmortem

An incident happened. Understand why, document it properly, and
make it harder to repeat. No hand-waving, no minimizing.

## Why This Exists

Bad postmortems dismiss ("no actual impact"), deflect ("edge case"),
or rush ("wrong command, fixed it, moving on"). These teach nothing.

A good postmortem makes the reader feel the weight of what could
have gone wrong, traces the failure to its root, and produces
concrete changes. It's a document you'd send to your team lead
without embarrassment.

## On Activation

Write the postmortem. Don't ask clarifying questions unless the
incident is genuinely ambiguous — you usually have full context
from the conversation. Output the complete document in one pass.

## Structure

Use every section. No section is optional. If a section seems
inapplicable, you haven't thought hard enough.

```markdown
## Postmortem: <Title>

**Date**: YYYY-MM-DD
**Severity**: Critical | High | Medium | Low
**Duration**: Time from incident to resolution
**Detection**: Who/what caught it (user, CI, automated check, self)

---

### Summary

Two to three sentences. What happened, what was the impact, how
was it resolved. A stranger should understand the incident from
this paragraph alone.

### Impact

**Actual**: What damage occurred. Be honest — if none, say none
and explain why (e.g., "user caught it before execution").

**Potential**: What would have happened if undetected. This is the
important part. Trace the counterfactual — how far would the
damage have propagated? What downstream decisions would have been
corrupted? How long until someone noticed?

### Timeline

Chronological sequence of events. Include timestamps or relative
ordering. Start from the triggering instruction, end at resolution.

| Time/Order | Event                |
| ---------- | -------------------- |
| T+0        | User instructs X     |
| T+1        | Agent does Y instead |
| T+2        | User catches error   |
| T+3        | Corrected to Z       |

### Root Cause

The deepest "why" you can reach. Not "I ran the wrong command" —
why did you run the wrong command? Pattern matching? Assumption?
Fatigue? Familiarity bias? Pressure to recover from a prior
mistake?

Use the 5 Whys if helpful:

1. Why did tests run against the wrong version?
2. Because the environment wasn't rebuilt after code changes.
3. Why wasn't it rebuilt?
4. Because I chose the fast-restart command over the full-rebuild command.
5. Why didn't I check whether a rebuild was needed?
6. Because I had no pre-flight checklist for build commands.

### Contributing Factors

Other conditions that made the failure more likely or more
dangerous. These aren't the root cause but they shaped the
incident. Examples:

- Session fatigue from prior errors
- No automated guard against stale binaries
- Time pressure (real or perceived)
- Ambiguity in the instruction (only if genuine)

### Lessons

What this incident teaches. Not platitudes ("be more careful")
— specific, falsifiable insights.

Bad: "I should read instructions more carefully."
Good: "After committing code changes, the environment must be
rebuilt before testing. A fast-restart command is never correct
when source has changed — it tests against the old build."

### Action Items

Concrete changes. Each item should be specific enough that you
could verify whether it was done.

- [ ] Before any restart/rebuild command, check: has source
      changed since the last build? If yes, use the full rebuild.
- [ ] Learn the project's build commands and when each applies.
```

## Principles

### Severity Calibration

| Severity | Criteria                                                          |
| -------- | ----------------------------------------------------------------- |
| Critical | Data loss, published incorrect content, irreversible action taken |
| High     | Silent correctness risk, stale data, action in wrong scope        |
| Medium   | Wrong output caught before use, wasted significant time           |
| Low      | Wrong command self-corrected, cosmetic error                      |

Severity is based on **potential** damage, not actual. A test
that passed against stale code is still High — the failure mode
is silent and the results would have been trusted.

### Detection Credit

Who caught it matters. If the user caught it, say so — that's
a failure of your own validation. If CI caught it, the system
worked. If you caught it yourself before any effect, note that
too. The goal is honest accounting of where the safety net was.

### No Minimizing

These phrases are banned in postmortems:

- "No actual impact" (as a way to close the discussion)
- "Edge case" (as an excuse)
- "Minor issue" (when the potential was not minor)
- "Already fixed" (without explaining what was fixed and why)
- "Won't happen again" (without explaining what changed)

Every one of these is a signal that the postmortem is being
written to close a ticket, not to learn from a failure.

### Compound Incidents

When multiple failures occur in one session, each gets its own
postmortem unless they share a root cause. If they share a root
cause, write one postmortem that covers all incidents and
explicitly names the shared root cause.

Look for escalation patterns: did the first failure create
pressure that caused the second? If so, the escalation itself
is a finding worth documenting.

### Proportionality

The postmortem's length should match the incident's severity
and instructional value. A Critical incident with a novel
failure mode deserves a full writeup. A Low severity typo
that was self-corrected needs a few sentences, not a page.

But when in doubt, err on the side of thoroughness. A
postmortem that's too detailed teaches something. A postmortem
that's too brief teaches nothing.
