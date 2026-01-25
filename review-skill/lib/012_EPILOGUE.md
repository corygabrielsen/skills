# Epilogue

**Report results and end.**

## Do:
- Report outcome
- End the skill

## Don't:
- Skip the completion message
- Continue after reporting

## Output Templates

**No issues found:**
```
No issues.
```

**Issues addressed:**
```
Review complete.
Issues: {fixed_count} fixed, {clarified_count} clarified (from {reviewers_with_issues} reviewers).
```

- `{fixed_count}` = issues where real problems were corrected
- `{clarified_count}` = false positives addressed by adding clarifying text
- `{reviewers_with_issues}` = count of reviewers that reported at least one issue
