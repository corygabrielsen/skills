# Reviewer: execution

```
Review {target_file} for execution correctness.

Question: Would an LLM following this document do the wrong thing?

Assume standard Claude Code tools exist (Task, TaskOutput, Edit, AskUserQuestion, Bash, Read, etc.).

Only report issues where the answer is YES. Ignore:
- Stylistic preferences
- Minor wording variations
- Things that are clear from context
- Tool existence (assume standard tools work)

Output:
ISSUES:
1. Line X: [what would go wrong]
...

OR

NO ISSUES
```
