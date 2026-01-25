# Loop Gate

**Check pass count and decide whether to loop or exit.**

## Do:
- Check if current pass < N
- If looping: increment pass counter, re-read target file, reset per-pass state, then jump to Fan Out
- If not looping: continue to Epilogue

## Don't:
- Skip re-reading the target file when looping (it has changed)
- Forget to clear issue tracker and reset reviewer results on loop
- Lose cumulative stats (total fixed/clarified counts across all passes)

## Logic:

```
IF pass < N:
    → Increment pass counter
    → Jump to Fan Out (phase 002)

OTHERWISE:
    → Continue to Epilogue
```

## On loop:
- Re-read the target file (it has changed)
- Reset per-pass state: clear issue tracker, reset reviewer results
- Preserve cumulative stats (total fixed/clarified counts across all passes)
