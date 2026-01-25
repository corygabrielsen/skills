---
name: quineify-review-skill
description: Run /review-skill on its own SKILL.md until it reaches a fixed point. Self-referential review loop.
---

# Quineify Review Skill

Run `/review-skill` on its own SKILL.md until it reaches a fixed point. A skill that reviews itself until it can't find anything to improve.

## Core Concept

```
/review-skill @review-skill/SKILL.md --auto
         │
         ▼
    ┌─────────┐
    │ Issues? │
    └────┬────┘
         │
    yes ─┴─ no
     │      │
     ▼      ▼
  repeat   done
```

This is a quine-like pattern: the review skill examining its own definition until stable.

---

## State

```yaml
max_iterations: 10        # Safety limit
iteration_count: 0        # Current iteration
target: "@review-skill/SKILL.md"
```

---

## Phase: Initialize

1. Set `iteration_count = 0`
2. Set `max_iterations = 10`
3. Confirm target exists: `/home/cory/code/claude-skills/review-skill/SKILL.md`

---

## Phase: Loop

```
while iteration_count < max_iterations:
    iteration_count += 1

    1. Run: /review-skill @review-skill/SKILL.md --auto
    2. Parse result
    3. If result is "No issues" or equivalent → FIXED POINT, exit loop
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
## Quineify Complete

| Metric | Value |
|--------|-------|
| Iterations | {iteration_count} |
| Fixed point reached | yes/no |
| Final state | clean / max iterations hit |
```

If fixed point reached:
> review-skill/SKILL.md has reached a fixed point after {N} iterations.
> The skill document is now internally consistent and self-evident.

If max iterations hit:
> Safety limit reached after {max_iterations} iterations.
> The document may still have issues. Consider increasing the limit or investigating.

---

## Quick Reference

| Phase | Action |
|-------|--------|
| Initialize | Set counters, verify target exists |
| Loop | Call /review-skill --auto, check for fixed point |
| Report | Show iteration count and final state |

---

Begin quineify-review-skill now. Initialize state. Enter loop: invoke `/review-skill @review-skill/SKILL.md --auto`, check result, repeat until fixed point or max iterations. Report final state.
