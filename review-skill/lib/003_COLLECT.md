# Collect

**Gather results from all reviewers.**

## Do:
- Use `TaskOutput` with `task_id: <id>` for each reviewer that successfully launched, **in a single assistant turn with up to 8 parallel TaskOutput calls** (if fewer than 8 launched, only call TaskOutput for those that did). TaskOutput blocks until completion, so this turn waits for all reviewers to finish.
- Parse each reviewer's output into tracker rows. For each numbered issue:
  - If it starts with "Line X:", extract X into Line column and the text after the colon into Issue column
  - If no "Line X:" prefix (some reviewers like checklist omit line numbers), use "-" for Line column
  - Assign sequential IDs (I-001, I-002...; IDs reset each pass), set Fix to "—" and Status to `open`

## Don't:
- Proceed before all reviewers complete
- Ignore any reviewer's issues

## Evaluate Results

**Valid reviewer output** is one of:
- Exactly `NO ISSUES` (reviewer found nothing)
- `ISSUES:` followed by numbered items (`1. ...`, `2. ...`, etc.)—items may or may not have "Line X:" prefix

**Malformed output** (neither pattern above) or **task execution error**: record a single tracker entry with Issue="Reviewer output error: [description]", Line="-". Proceed to Synthesize.

```
if ALL successfully-launched reviewers output NO ISSUES AND no launch failures were recorded:
    → Skip Synthesize through Commit; proceed directly to Epilogue
else:
    → Merge issues into tracker
    → Proceed to Synthesize
```

If any reviewer failed to launch, always proceed to Synthesize even if remaining reviewers found no issues. (Launch failures are infrastructure errors recorded in the tracker for visibility, not document issues to address.)

## Issue Tracker Format

```markdown
| ID | Reviewer | Line | Issue | Fix | Status |
|:--:|:--------:|:----:|:------|:----|:------:|
| I-001 | execution | 42 | [description] | — | open |
| I-002 | coverage | 156 | [description] | Added X | clarified |
```

- **Line**: Line number from reviewer output. For multi-line issues (e.g., contradictions reporting "Lines X and Y"), use the first line number; mention additional line numbers in the Issue description (e.g., "Lines 42 and 78 contradict...").
- **Issue**: Brief description of what was flagged
- **Fix**: Short prose snippet of change made (use "—" while `open` or `planned`)

**Status progression:** `open` → `planned` (in Triage) → `fixed` or `clarified` (in Address)

- `fixed` = real issue was corrected
- `clarified` = false positive addressed by adding clarifying text
