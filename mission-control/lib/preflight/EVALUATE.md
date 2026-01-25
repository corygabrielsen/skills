# Pre-Flight: Evaluate

**Run go/no-go checks on all ready tasks.**

## Do:
- Identify all ready tasks (pending, empty blockedBy)
- For each ready task, evaluate against checklist
- Record GO or NO-GO with reason for each
- Produce evaluation table

## Don't:
- Skip any ready task
- Auto-fix issues (that's the FIX phase)
- Proceed past NO-GOs

## Checklist

| Check | Question | NO-GO if... |
|-------|----------|-------------|
| Scope | Is the task well-defined? | Agent would need to guess intent |
| Context | Does description have what agent needs? | Missing files, unclear references |
| Criteria | How do we verify success? | No way to validate completion |
| Dependencies | Are blockers actually resolved? | Upstream task failed/incomplete |
| Size | Can one agent complete this? | Task too large, needs decomposition |

## Output Format

```markdown
## Pre-Flight Evaluation

| Task | Scope | Context | Criteria | Deps | Size | Status |
|------|-------|---------|----------|------|------|--------|
| T-001 | GO | GO | GO | GO | GO | **GO** |
| T-002 | GO | NO-GO | GO | GO | GO | **NO-GO** |
| T-003 | GO | GO | GO | GO | NO-GO | **NO-GO** |

### NO-GO Details
- T-002: Context - Description references "the auth file" without path
- T-003: Size - Task includes 5 distinct features, needs decomposition
```

## Transitions

```
if all ready tasks are GO:
    → exit preflight, proceed to execution/DELEGATE

if any task is NO-GO:
    → proceed to preflight/HIL_HOLD
```
