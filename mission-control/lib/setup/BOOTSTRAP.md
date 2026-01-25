# Bootstrap

**Mine existing conversation to propose initial task graph.**

Only runs when: TaskList is empty AND conversation has history.

## Do:

- Mine the conversation for:
  - Work already completed (note for context, not task creation)
  - Explicit user requests
  - Implicit tasks ("we should also...", "don't forget to...")
  - Decisions made (capture in descriptions)
  - Blockers or open questions
  - Work started but not finished
  - Dependencies between items
- **Propose** tasks in markdown format (do NOT call TaskCreate yet)
- Write descriptions detailed enough to survive compaction
- Report proposed graph as status table

## Don't:

- Call TaskCreate (that's MATERIALIZE's job, after approval)
- Ask "would you like me to create tasks?"---that's why mission control was summoned
- Create vague or context-dependent task descriptions
- Skip completed work context (it shows momentum)

## Output Format

```markdown
## Proposed Task Graph

### Context (Completed Work)
| # | Description |
|---|-------------|
| - | [what was already done, for reference] |

### Proposed Tasks
| # | Subject | Description | Blocked By |
|---|---------|-------------|------------|
| 1 | [subject] | [what needs doing] | --- |
| 2 | [subject] | [depends on 1] | 1 |
```

After proposing, proceed to HIL_GATE_PLAN_APPROVAL for user approval.
