# Initialize

**Parse args and load current state.**

## Do:
- Run `TaskList` to see current task state
- Parse args for mode flags: `--fg`, `--bg`, `--auto`
- Default to `--bg` if no mode flag specified
- Store mode in working memory for later phases

## Don't:
- Skip TaskList check
- Assume mode without checking args

## Args:
- `--fg`: Foreground mode. Launch agents but block on them.
- `--bg`: Background mode (default). Launch agents and return control to human.
- `--auto`: Skip human checkpoints in foreground mode.

## State Check

```
if TaskList returns tasks:
    Check task states and route:

    if any tasks are in_progress:
        → execution/MONITOR (agents are running)

    else if any tasks are pending with empty blockedBy:
        → preflight/EVALUATE (ready tasks waiting)

    else if any tasks are pending but all blocked:
        → control/REPORT (show blocked status)

    else if all tasks completed or ABORTED:
        → control/REPORT (show final status)

else if conversation has history:
    → setup/BOOTSTRAP (mine conversation for work)

else:
    → setup/DECOMPOSE (fresh start, await user request)
```

## Handoff Detection

If a task with `metadata.type: "handoff"` exists, read it first to recover mission state (mode, decisions, open questions).
