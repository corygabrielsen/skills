---
name: loop-review-skill-until-fixed-point
description: Iterate /review-skill on a target until fixed point. Runs review passes until all reviewers return NO ISSUES.
---

# Loop Review Skill Until Fixed Point

Run `/review-skill` on a target document repeatedly until fixed point—when all reviewers return NO ISSUES.

## Core Concept

```
/review-skill <target> --auto
         │
         ▼
    ┌─────────┐
    │ Issues? │
    └────┬────┘
         │
    yes ─┴─ no
     │      │
     ▼      ▼
  repeat   done (fixed point)
```

Fixed point = the document is both correct AND unambiguous. No reviewer can find anything to flag.

---

## Arguments

| Arg | Required | Description |
|-----|----------|-------------|
| `<target>` | yes | Path to SKILL.md to review |

---

## State

```yaml
max_iterations: 10        # Safety limit
iteration_count: 0        # Current iteration
target: "<from args>"     # Target SKILL.md path
```

---

## Phase: Initialize

1. Parse target path from arguments
2. Set `iteration_count = 0`
3. Set `max_iterations = 10`
4. Confirm target exists

---

## Phase: Loop

```
while iteration_count < max_iterations:
    iteration_count += 1

    1. Run: /review-skill <target> --auto
    2. Parse result
    3. If all reviewers return "NO ISSUES" → FIXED POINT, exit loop
    4. Else → issues were addressed, continue loop
```

### Exit Conditions

| Condition | Action |
|-----------|--------|
| All reviewers return "NO ISSUES" | Fixed point reached, exit with success |
| `iteration_count >= max_iterations` | Safety limit hit, ask user how to proceed |

---

## Phase: Report

Present final state:

```markdown
## Loop Complete

| Metric | Value |
|--------|-------|
| Target | {target} |
| Iterations | {iteration_count} |
| Fixed point reached | yes/no |
| Final state | clean / max iterations hit |
```

If fixed point reached:
> {target} has reached a fixed point after {N} iterations.
> The document is now internally consistent and unambiguous.

If max iterations hit:
> Safety limit reached after {max_iterations} iterations.
> The document may still have issues. Consider increasing the limit or investigating.

---

## Quick Reference

| Phase | Action |
|-------|--------|
| Initialize | Parse target, set counters, verify target exists |
| Loop | Call /review-skill --auto, check for fixed point |
| Report | Show iteration count and final state |

---

Begin now. Parse target from arguments. Initialize state. Enter loop: invoke `/review-skill <target> --auto`, check result, repeat until fixed point or max iterations. Report final state.
