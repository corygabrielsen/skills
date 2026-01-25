# Materialize

**Create tasks from approved plan.**

Only runs after HIL_GATE_PLAN_APPROVAL returns `approve`.

## Input

Proposed plan from BOOTSTRAP or DECOMPOSE (markdown table in context).

## Do:

- Create tasks via `TaskCreate` for each item in approved plan
- Set up dependencies with `TaskUpdate` + `addBlockedBy`
- Create handoff task with mission metadata (`metadata.type: "handoff"`, `metadata.mode`)
- Report created task IDs mapped to plan items

## Don't:

- Re-propose or modify the plan (that's done, user approved)
- Skip any items from the approved plan
- Create tasks that weren't in the plan

## Output Format

```markdown
## Tasks Created

| Plan # | Task ID | Subject |
|--------|---------|---------|
| 1 | T-001 | [subject] |
| 2 | T-002 | [subject] |
| 3 | T-003 | [subject] |

Dependencies configured. Proceeding to preflight.
```

## Next

→ preflight/PHASE
