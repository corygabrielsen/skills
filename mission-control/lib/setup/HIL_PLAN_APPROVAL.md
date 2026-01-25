# HIL: Plan Approval

**Human approves task breakdown before execution.**

**If `--auto` mode:** Show decomposition summary, then proceed directly to Pre-Flight.

## Do:
- Present task graph with dependencies
- Show each task's subject, description summary, blockedBy
- Highlight parallel vs sequential execution paths
- Use AskUserQuestion with clear options
- Wait for explicit approval

## Don't:
- Proceed to Pre-Flight before approval (unless `--auto`)
- Launch any agents before approval
- Assume approval

## Presentation Format

```markdown
## Proposed Task Graph

| Task | Subject | Blocked By | Parallel Group |
|------|---------|------------|----------------|
| T-001 | [subject] | --- | A |
| T-002 | [subject] | --- | A |
| T-003 | [subject] | T-001, T-002 | B |

Execution plan:
1. Group A: T-001, T-002 (parallel)
2. Group B: T-003 (after A completes)
```

## Options

```
AskUserQuestion(
  questions: [{
    question: "Approve this task breakdown?",
    header: "Plan",
    options: [
      {label: "Approve", description: "Proceed to execution"},
      {label: "Modify", description: "I'll adjust the tasks"},
      {label: "Abort", description: "Cancel mission"}
    ],
    multiSelect: false
  }]
)
```

## Handlers

**If user selects "Approve":** Proceed to Pre-Flight phase.

**If user selects "Modify":**
1. Prompt: "Describe what changes you'd like to the task breakdown."
2. End turn, wait for user input.
3. Update tasks via TaskUpdate or create new tasks.
4. Re-present Plan Approval (loop until Approve or Abort).

**If user selects "Abort":**
1. Mark all pending tasks as `ABORTED - User cancelled`.
2. End skill.
