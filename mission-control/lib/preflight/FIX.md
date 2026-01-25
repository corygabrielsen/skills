# Pre-Flight: Fix

**Resolve NO-GO conditions.**

## Entry
- User selected "Fix" in HIL_HOLD phase
- Have list of NO-GO tasks with reasons

## Do:
- For each NO-GO, apply appropriate fix
- Use TaskUpdate to improve task descriptions
- Split oversized tasks via new TaskCreate
- Document what was fixed

## Don't:
- Skip any NO-GO
- Make substantive changes without clear reasoning
- Create circular dependencies when splitting

## Fix Actions by NO-GO Type

| NO-GO Reason | Fix Action |
|--------------|------------|
| Scope unclear | Clarify task subject and description |
| Context missing | Add file paths, references, examples to description |
| Criteria undefined | Add "Done when:" section to description |
| Dependencies unresolved | Check upstream task, may need to wait or replan |
| Size too large | Split into smaller tasks with dependencies |

## Splitting Tasks

When a task is too large:

```
Original: T-003 "Implement auth system"

Split into:
  T-003a "Implement auth middleware" (blockedBy: [])
  T-003b "Implement login endpoint" (blockedBy: [T-003a])
  T-003c "Implement session management" (blockedBy: [T-003a])
  T-003d "Add auth tests" (blockedBy: [T-003b, T-003c])

Mark T-003 as: ABORTED - Split into T-003a, T-003b, T-003c, T-003d
```

## Output Format

```markdown
## Fixes Applied

| Task | Issue | Fix Applied |
|------|-------|-------------|
| T-002 | Context missing | Added explicit file path to description |
| T-003 | Too large | Split into 4 sub-tasks |
```

## Transitions

```
after fixes applied:
    → preflight/EVALUATE (re-check all ready tasks)
```
