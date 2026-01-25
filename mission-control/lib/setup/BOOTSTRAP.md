# Bootstrap

**Mine existing conversation to build initial task graph.**

Only runs when: TaskList is empty AND conversation has history.

## Do:
- Mine the conversation for:
  - Work already completed (create and immediately mark completed; see MR-3 exception in RULES.md)
  - Explicit user requests
  - Implicit tasks ("we should also...", "don't forget to...")
  - Decisions made (capture in task descriptions)
  - Blockers or open questions
  - Work started but not finished
  - Dependencies between items
- Create tasks via `TaskCreate`
- Mark completed work as completed via `TaskUpdate`
- Set up dependencies with `TaskUpdate` + `addBlockedBy`
- Write descriptions detailed enough to survive compaction
- Report catalog as status table (completed first, then pending)

## Don't:
- Ask "would you like me to create tasks?"---that's why mission control was summoned
- Create vague or context-dependent task descriptions
- Skip completed work (it shows momentum and leaves a trail)

## Output Format

```markdown
## Bootstrapped Task Graph

### Completed
| Task | Description |
|------|-------------|
| T-001 | [what was done] |

### Pending
| Task | Description | Blocked By |
|------|-------------|------------|
| T-002 | [what needs doing] | --- |
| T-003 | [depends on T-002] | T-002 |
```

After reporting, proceed to HIL_PLAN_APPROVAL for user approval of the bootstrapped graph.
