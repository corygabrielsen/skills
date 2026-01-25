# Pre-Flight

**Go/No-Go checks before launching agents.**

NASA's Flight Director polls every station before critical operations. One NO-GO stops everything.

## Do:
- For each ready task (empty blockedBy), run go/no-go checklist
- Record GO or NO-GO with reason
- If any NO-GO: fix before proceeding
- Only proceed to Delegate when all ready tasks are GO

## Don't:
- Skip checks "because it looks fine"
- Launch tasks that are NO-GO
- Proceed with unresolved NO-GOs

## Go/No-Go Checklist

For each ready task, evaluate:

| Check | Question | NO-GO if... |
|-------|----------|-------------|
| Scope | Is the task well-defined? | Agent would need to guess intent |
| Context | Does description have what agent needs? | Missing files, unclear references |
| Criteria | How do we verify success? | No way to validate completion |
| Dependencies | Are blockers actually resolved? | Upstream task failed or incomplete |
| Size | Can one agent complete this? | Task too large, needs decomposition |

## Evaluation Format

```markdown
## Pre-Flight Check

| Task | Scope | Context | Criteria | Deps | Size | Status |
|------|-------|---------|----------|------|------|--------|
| T-001 | GO | GO | GO | GO | GO | **GO** |
| T-002 | GO | NO-GO | GO | GO | GO | **NO-GO** |

T-002 NO-GO reason: Description references "the auth file" without specifying path.
```

## NO-GO Resolution

```
if any task is NO-GO:
    1. Report the NO-GO reasons
    2. Fix via TaskUpdate (improve description, clarify scope, etc.)
    3. Re-run Pre-Flight for affected tasks
    4. Repeat until all ready tasks are GO
```

## After Pre-Flight

```
if all ready tasks are GO:
    → Proceed to Delegate
else:
    → Fix NO-GOs, re-check
```
