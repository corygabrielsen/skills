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

## Args

| Args Passed | Effective Mode |
|-------------|----------------|
| (none) | `--bg` |
| `--fg` | `--fg` |
| `--bg` | `--bg` |
| `--fg --bg` | `--fg` |
| `--auto` | `--fg --auto` |
| `--auto --bg` | `--fg --auto` |
| `--fg --auto` | `--fg --auto` |

**Note:** `--auto` only applies to foreground mode. Background mode inherently returns control to human.

## Handoff Detection (First)

Before routing, check for handoff task: if any task has `metadata.type: "handoff"`, read it to recover mission state (mode, decisions, open questions).

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
    "Work-related history" triggers BOOTSTRAP if conversation contains ANY of:
    - Explicit action requests ("implement X", "fix Y", "create Z", "add a...")
    - File paths or code snippets discussed
    - Decisions recorded ("let's use X", "we decided Y")
    - Work already done that should be catalogued
    If user only exchanged greetings or asked general questions with no action
    requests, treat as fresh start → DECOMPOSE.

else:
    → setup/DECOMPOSE (fresh start, await user request)
```
