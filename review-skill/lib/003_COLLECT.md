# Collect

**Gather results from all reviewers.**

## Do:
- Use `TaskOutput` with `task_id: <id>` for each reviewer that successfully launched, **in a single assistant turn**, to wait for completion (if fewer than 7 launched, only call TaskOutput for those that did)
- Parse each reviewer's output: extract line number from "Line X:" prefix into Line column, extract description after the colon into Issue column, assign sequential IDs (I-001, I-002...), set Fix to "—" and Status to `open`. Note: Some reviewers (e.g., checklist) output issues without line numbers—use "-" for the Line column when no line number is present.

## Don't:
- Proceed before all reviewers complete
- Ignore any reviewer's issues

## Evaluate Results

A reviewer has no issues if its output contains `NO ISSUES`. Treat malformed output (neither "NO ISSUES" nor a recognizable "ISSUES:" list format) or failed reviewer output (task execution error) as having issues—record "Reviewer output error: [error description]" in the Issue field (use "-" for Line column) and follow the normal issue path (proceed to Synthesize). Do not attempt partial parsing of malformed output; treat the entire response as a single error entry.

```
if ALL 7 reviewers output NO ISSUES AND no launch failures were recorded in the tracker:
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

- **Line**: Line number from reviewer output. For multi-line issues (e.g., contradictions reporting "Lines X and Y"), use the first line number; include additional lines in the Issue description if needed.
- **Issue**: Brief description of what was flagged
- **Fix**: Short prose snippet of change made (use "—" while `open` or `planned`)

**Status progression:** `open` → `planned` (in Triage) → `fixed` or `clarified` (in Address)

- `fixed` = real issue was corrected
- `clarified` = false positive addressed by adding clarifying text
