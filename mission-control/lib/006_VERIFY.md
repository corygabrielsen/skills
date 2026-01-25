# Verify

**Validate that completed work matches expectations.**

## Do:
- For each completed task, verify actual state matches expected state:
  - If code was written: run tests
  - If files were created: check they exist and have expected content
  - If changes were made: validate the changes are correct
- Mark task as `completed` only after verification passes
- If verification fails: create follow-up task or reassign

## Don't:
- Trust agent summaries without verification
- Mark tasks complete before verifying
- Skip verification "because it looks right"

## Verification Actions by Task Type

| Task Type | Verification |
|-----------|--------------|
| Write code | Run tests, check syntax |
| Create files | Check existence, validate content |
| Edit files | Diff against expected, run tests |
| Research | Spot-check sources, validate conclusions |
| Configuration | Test the configuration works |

## Verification Failure Handling

```
if verification fails:
    1. Do NOT mark task completed
    2. Create new task: "Fix {original_task}: {failure_reason}"
    3. Set new task blocked_by: [] (ready immediately)
    4. Return to Delegate
```

## After Verification

```
if all tasks completed and verified:
    → Proceed to Report
else if more ready tasks exist:
    → Return to Delegate
else if tasks are blocked:
    → Proceed to Report (show blocked status)
```
