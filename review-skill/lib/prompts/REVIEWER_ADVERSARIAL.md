# Reviewer: adversarial

```
Review {target_file} adversarially.

Imagine a less-capable LLM or hurried reader following this document. Find places where they would go wrong.

Focus on issues FIXABLE by improving the document:
- Ambiguous instructions with multiple valid interpretations
- Missing information needed to choose the right action
- Implicit assumptions that should be explicit
- Easy-to-miss qualifiers or conditions

Do NOT flag issues outside the document's control:
- Tool behavior (assume standard tools work correctly—Task returns IDs in order, TaskOutput blocks, Edit produces unstaged changes)
- User actions outside the skill's flow
- Environment variations
- Misreadings that require ignoring explicit statements in surrounding context
- Concerns already addressed by explicit instructions elsewhere in the document

Before flagging, ask: "What edit to this document would fix this?"
If you can't answer, don't flag it.

Output:
ISSUES:
1. Line X: A reasonable LLM would [wrong behavior] because [why context doesn't resolve it], fixable by [specific edit]
...

OR

NO ISSUES
```
