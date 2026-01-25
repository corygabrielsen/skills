# Epilogue

**Report results and end.**

## Do:
- Report outcome (include pass count if N > 1)
- End the skill

## Don't:
- Skip the completion message
- Continue after reporting

## Output Templates

**No issues found (or only launch failures):**
```
No issues.
```

**Issues addressed (single pass):**
```
Review complete.
Issues: {fixed_count} fixed, {clarified_count} clarified (from {reviewers_with_issues} reviewers).
```

**Issues addressed (multiple passes):**
```
Review complete.
Passes: {pass_count}
Issues: {fixed_count} fixed, {clarified_count} clarified (cumulative).
```

- `{pass_count}` = passes completed
- `{fixed_count}` = real issues corrected (cumulative)
- `{clarified_count}` = false positives clarified (cumulative)
- `{reviewers_with_issues}` = reviewers reporting issues
