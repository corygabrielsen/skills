# Loop Gate

**Check pass count and decide whether to loop or exit.**

## Do:
- Check if current pass < N
- If looping: (1) increment pass counter, (2) re-read target file, (3) reset per-pass state (clear tracker and reviewer results), (4) jump to Fan Out
- If not looping: continue to Epilogue

## Don't:
- Skip re-reading the target file when looping (it has changed)
- Forget to clear tracker and reviewer results on loop
- Lose cumulative stats (fixed/clarified counts across passes)

## Logic:

```
IF pass < N:
    → Increment pass counter
    → Jump to Fan Out (phase 002)

OTHERWISE:
    → Continue to Epilogue
```
