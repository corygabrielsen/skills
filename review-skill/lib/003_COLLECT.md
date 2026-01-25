# Collect

**Gather results from all reviewers.**

## Do:
- Use `TaskOutput` with `task_id: <id>` for each reviewer, **in a single assistant turn (6 parallel TaskOutput calls)**, to wait for completion
- Parse each reviewer's output format

## Don't:
- Proceed before all reviewers complete
- Ignore any reviewer's issues

## Evaluate Results

A reviewer has no issues if its output contains `NO ISSUES`. Treat malformed output (neither "NO ISSUES" nor a recognizable "ISSUES:" list format) or failed reviewer output (task execution error) as having issues—record "Reviewer failed: [error]" in the Issue field (use "-" for Line column) and follow the normal issue path (proceed to Synthesize).

```
if ALL 6 reviewers output NO ISSUES:
    → Proceed to Epilogue (no-issues path)
else:
    → Merge issues into tracker
    → Proceed to Synthesize
```

## Issue Tracker Format

```markdown
| ID | Reviewer | Line | Issue | Fix | Status |
|:--:|:--------:|:----:|:------|:----|:------:|
| I-001 | execution | 42 | [description] | — | open |
| I-002 | coverage | 156 | [description] | Added X | clarified |
```

- **Line**: Line number from reviewer output. For multi-line issues (e.g., contradictions reporting "Lines X and Y"), use the first line number or format as "X,Y".
- **Issue**: Brief description of what was flagged
- **Fix**: Short prose snippet of change made (use "—" while `open` or `planned`)

**Status progression:** `open` → `planned` (in Triage) → `fixed` or `clarified` (in Address)

- `fixed` = real issue was corrected
- `clarified` = false positive addressed by adding clarifying text
