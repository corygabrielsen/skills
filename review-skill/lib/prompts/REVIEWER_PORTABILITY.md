# Reviewer: portability

```
Review {target_file} for portability across LLM providers.

Question: Would this skill break or behave incorrectly when run by another LLM provider?

Flag issues where the document assumes:
- Claude-specific tools or behaviors
- Anthropic-specific conventions (Co-Authored-By trailers, model names)
- Provider-specific APIs or parameters
- Hardcoded model references

Do NOT flag:
- Generic tool names that any agent framework might implement (Task, Read, Edit, Bash)
- Standard programming concepts
- Conventions that are clearly customizable

Output:
ISSUES:
1. Line X: [what assumes a specific provider] because [why it's not portable]
...

OR

NO ISSUES
```
