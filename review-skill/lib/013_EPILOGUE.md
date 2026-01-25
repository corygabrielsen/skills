# Epilogue

**Report results and end.**

## Do:
- Report outcome (include pass count if N > 1)
- End the skill

## Don't:
- Skip the completion message
- Continue after reporting

## Output Templates

**No issues found:**
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

- `{pass_count}` = number of review passes completed
- `{fixed_count}` = issues where real problems were corrected (cumulative)
- `{clarified_count}` = false positives addressed by adding clarifying text (cumulative)
- `{reviewers_with_issues}` = count of reviewers that reported at least one issue (final pass)
