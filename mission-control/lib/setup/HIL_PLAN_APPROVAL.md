# HIL: Plan Approval

**Human approves task breakdown before execution.**

**If `--auto` mode:** Show decomposition summary, then proceed directly to preflight.

## Do:
- Present task graph with dependencies
- Show each task's subject, description summary, blockedBy
- Highlight parallel vs sequential execution paths
- Use AskUserQuestion with clear options
- Wait for explicit approval

## Don't:
- Proceed to preflight before approval (unless `--auto`)
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

**Fallback:** If `AskUserQuestion` is unavailable, present options as a numbered list and wait for user to respond with their choice number or label.

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

**If user selects "Approve":** Proceed to preflight phase.

**If user selects "Modify":**
1. Prompt: "Describe what changes you'd like to the task breakdown."
2. End turn, wait for user input.
3. If user provides empty/unclear input or cancels ("nevermind"), re-present the same Plan Approval options (no changes made).
4. Otherwise, interpret user feedback and make appropriate TaskUpdate calls or create new tasks. If interpretation is ambiguous, ask a clarifying question before editing.
5. Re-present Plan Approval with updated graph (loop until Approve or Abort).

**If user selects "Abort":**
1. Mark all pending tasks as `ABORTED - User cancelled`.
2. End skill.
