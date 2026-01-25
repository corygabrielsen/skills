# Verify

**Validate that completed work matches expectations.**

## Do:
- For each completed task, verify actual state matches expected state:
  - If code was written: run tests
  - If files were created: check they exist and have expected content
  - If changes were made: validate the changes are correct
- Mark task as `completed` only after verification passes
- If verification fails: proceed to control/HIL_ANOMALY

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
    2. Record failure details
    3. → control/HIL_ANOMALY (human decides response)
```

Do NOT automatically create follow-up tasks. Let control/HIL_ANOMALY handle failure classification and response.

## After Verification

```
if verification passed for all checked tasks:
    Mark each as `completed`
    → control/CHECKPOINT

if verification failed for any task:
    Mark passed tasks as `completed` (they succeeded)
    Do NOT mark failed tasks
    → control/HIL_ANOMALY (for failed tasks only)
```

**Mixed results:** When some tasks pass and some fail, mark passed tasks complete first, then proceed to HIL_ANOMALY for the failed ones. Passed work is preserved. Completed tasks persist in task system and appear in subsequent CHECKPOINT/REPORT phases regardless of HIL_ANOMALY routing for failed tasks.
