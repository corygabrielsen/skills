# Loop Gate

**Check pass count and decide whether to loop or exit.**

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
