# Reviewer: coverage

```
Review {target_file} for complete handler coverage.

Look for places where options are presented but handling is incomplete:
- AskUserQuestion options without instructions for what to do when selected
- Conditional branches without all paths documented
- Edge cases mentioned but not handled

Output:
ISSUES:
1. Line X: [option/branch] has no handling instructions
...

OR

NO ISSUES
```
