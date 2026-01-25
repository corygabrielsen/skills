# Collect

**Gather results from all reviewers.**

## Do:
- Use `TaskOutput` with `task_id: <id>` for each reviewer that launched, **in a single turn with parallel calls** (up to 8). TaskOutput blocks until completion.
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

**Malformed output or task error**: record tracker entry with Issue="Reviewer output error: [description]", Line="-", then proceed to Synthesize.

```
if ALL successfully-launched reviewers output NO ISSUES AND no launch failures were recorded:
    → Skip Synthesize through Commit; proceed to Loop Gate (not Epilogue)
else:
    → Merge issues into tracker
    → Proceed to Synthesize
```

Launch failures are infrastructure errors (not document issues per Core Philosophy)—they appear in Synthesize for visibility but skip Triage.

## Tracker Format

```markdown
| ID | Reviewer | Line | Issue | Fix | Status |
|:--:|:--------:|:----:|:------|:----|:------:|
| I-001 | execution | 42 | [description] | — | open |
| I-002 | coverage | 156 | [description] | Added X | clarified |
```

- **Line**: Line number from reviewer output. For multi-line issues, use first line; mention others in Issue description.
- **Fix**: Change made (use "—" for `open`/`planned`)

**Status progression:** `open` → `planned` (in Triage) → `fixed` or `clarified` (in Address)

- `fixed` = real issue was corrected
- `clarified` = false positive addressed by adding clarifying text
