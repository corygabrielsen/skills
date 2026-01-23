---
name: debrief
description: Reconstruct context for a returning human, synthesize status, and prompt for direction via structured Q&A. Minimizes cognitive load - they just scan and pick.
---

# Debrief

You are debriefing your commander who just returned and has limited memory of where things stand. Do the cognitive work so they can just scan, pick, and move on.

## The Pattern

The human is delegating not just work, but **cognitive labor**:

| Burden | You handle it |
|--------|---------------|
| Recall | What did they ask you to do? |
| Tracking | What's done vs pending? |
| Analysis | What are the options now? |
| Framing | How should they think about the decision? |

Human's remaining role: **scan, pick, confirm**

## Procedure

### 1. Reconstruct Context (offload MEMORY)

Mine the conversation and task system for:
- **Original goal** — what were we trying to accomplish?
- **Key decisions** — choices that shaped direction
- **Work completed** — what's done and verified
- **Work in progress** — started but not finished
- **Blockers / open questions** — what stopped progress or remains ambiguous
- **Relevant artifacts** — files, branches, URLs

Present as 2-4 bullet points. Not a wall of text.

### 2. Synthesize Status (offload TRACKING)

Check `TaskList` if tasks exist. Present a scannable report:

```
**Completed:** Task A, Task B
**In Progress:** Task C (was working on X)
**Blocked:** Task D — waiting on [blocker]
**Open Questions:** Question 1?
```

Human should grasp status in 10 seconds. Use a table if helpful.

### 3. Generate Options (offload ANALYSIS)

Identify 2-4 reasonable next actions. YOU figure out what the options are.

**Good options:**
- "Continue implementing feature X (pick up where we left off)"
- "Fix the failing test in auth.spec.ts first"
- "Revisit the architecture decision about Y"
- "Wrap up and summarize what we learned"

**Bad options:**
- "Continue?" (too vague)
- "What would you like to do?" (makes them think)
- Five similar variations (decision paralysis)

### 4. Prompt via Q&A (offload DECISION-FRAMING)

Use `AskUserQuestion` with clear options. Human should be able to pick without reading anything else.

Include:
- A recommended option (mark it)
- Brief description of each option's implication
- "Something else" escape hatch (automatic)

### 5. After They Choose

Once the human picks a direction:
- **Immediately act** on their choice
- Don't ask for confirmation
- Don't re-explain what you're about to do
- Just do it

## Output Format

```
**Where we left off:**
- [Original goal in one sentence]
- [Key decision or state]
- [Current blocker or next milestone]

**Status:**
| Done | In Progress | Blocked |
|------|-------------|---------|
| X, Y | Z | - |

**Recommendation:** [Your suggested next step and why, 1-2 sentences]

[AskUserQuestion with 2-4 options]
```

## Example

```
**Where we left off:**
- Building release pipeline for `tint` (terminal color picker)
- Decided on v0.1.0-alpha.1 as first release
- All CI/CD infrastructure merged to dev branch

**Status:**
| Done | Pending |
|------|---------|
| install.sh, CHANGELOG, CI workflow, tests | Draft release |

**Recommendation:** The release infrastructure is tested and ready. I'd create the tag and draft release now.

[AskUserQuestion: "What's next?"
- "Create v0.1.0-alpha.1 release" (recommended)
- "Review the CI/CD setup first"
- "Switch to something else"]
```

## Edge Cases

**Nothing to debrief:**
If the conversation just started or has no meaningful history:
- Say so clearly: "This conversation just started — nothing to debrief yet."
- Ask what they'd like to work on

**Task system exists:**
Always check `TaskList`. Tasks persist across compactions and may have state the conversation doesn't.

## Anti-patterns

- Wall of text (human has to read too much)
- "What would you like to do?" without options (human has to think)
- Dumping task list without synthesis (human has to analyze)
- Too many options (decision fatigue)
- Asking for confirmation after they already chose
- Forgetting to check TaskList
- Skipping the recommendation

---

Execute debrief now. Scan the conversation, check TaskList, synthesize status, and present options via `AskUserQuestion`.
