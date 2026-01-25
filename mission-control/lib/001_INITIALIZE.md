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
    → Load existing state, proceed to Monitor (check in-progress tasks)
else if conversation has history:
    → Proceed to Bootstrap (mine conversation for work)
else:
    → Proceed to Decompose (fresh start, await user request)
```
