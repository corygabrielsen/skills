# Initialize

**Parse args and load current state.**

## Do:
- Run `TaskList` to see current task state
- Parse args for mode flags: `--fg`, `--bg`, `--auto`
- Default to `--bg` if no mode flag specified
- Store mode on the handoff task (`metadata.mode`). If no handoff task exists yet, create one during DECOMPOSE or first HANDOFF. Read it back after compaction via `metadata.type: "handoff"`.

## Don't:
- Skip TaskList check
- Assume mode without checking args

## Args:
- `--fg`: Foreground mode. Launch agents but block on them.
- `--bg`: Background mode (default). Launch agents and return control to human.
- `--auto`: Skip human checkpoints. Requires `--fg`; if used with `--bg`, treat as `--fg --auto`.
- If both `--fg` and `--bg` are passed explicitly, `--fg` wins (explicit foreground overrides default).

**Note:** `--auto` only applies to foreground mode. Background mode inherently returns control to human.

## State Check

```
if TaskList returns tasks:
    Check task states and route (in priority order):

    if any tasks are in_progress:
        → execution/MONITOR (check on running agents first)
        Note: If ready tasks also exist, MONITOR will cycle back to delegate them

    else if any tasks are pending with empty blockedBy:
        → preflight/EVALUATE (ready tasks waiting)

    else if any tasks are pending but all blocked:
        → control/REPORT (show blocked status)

    else if all tasks completed or ABORTED:
        → control/REPORT (show final status)

else if conversation has work-related history:
    → setup/BOOTSTRAP (mine conversation for work)
    "Work-related history" means: technical discussion, task mentions, decisions,
    or context beyond greetings/skill invocation. If user only said "hi" then
    "/mission-control", that's a fresh start, not bootstrap.

else:
    → setup/DECOMPOSE (fresh start, await user request)
```

## Handoff Detection

If a task with `metadata.type: "handoff"` exists, read it first to recover mission state (mode, decisions, open questions).
