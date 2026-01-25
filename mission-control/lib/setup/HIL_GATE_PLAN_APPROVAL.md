# HIL Gate: Plan Approval

**Pure gate: Human approves proposed task breakdown.**

No side effects. Returns decision only.

**If `--auto` mode:** Skip gate, return `approve` decision.

## Input

Expects a proposed plan from BOOTSTRAP or DECOMPOSE (markdown table in context, no tasks created yet).

## Do:

- Present proposed task graph with dependencies
- Show each task's subject, description summary, blockedBy
- Highlight parallel vs sequential execution paths
- Use AskUserQuestion with clear options
- Wait for explicit approval
- Return decision to caller

## Don't:

- Create tasks (that's MATERIALIZE's job)
- Modify any state
- Proceed to preflight directly (caller handles routing)

## Presentation Format

```markdown
## Proposed Task Graph

| # | Subject | Blocked By | Parallel Group |
|---|---------|------------|----------------|
| 1 | [subject] | --- | A |
| 2 | [subject] | --- | A |
| 3 | [subject] | 1, 2 | B |

Execution plan:
1. Group A: Tasks 1, 2 (parallel)
2. Group B: Task 3 (after A completes)
```

## Options

```
AskUserQuestion(
  questions: [{
    question: "Approve this task breakdown?",
    header: "Plan",
    options: [
      {label: "Approve", description: "Proceed to create tasks and execute"},
      {label: "Modify", description: "I'll adjust the plan"},
      {label: "Abort", description: "Cancel mission"}
    ],
    multiSelect: false
  }]
)
```

**Fallback:** If `AskUserQuestion` is unavailable, present options as a numbered list and wait for user response.

## Returns

| Decision | Next Step |
|----------|-----------|
| `approve` | → MATERIALIZE (create tasks, then preflight) |
| `modify` | → Re-propose (DECOMPOSE or inline edit) |
| `abort` | → End skill |

## Modify Flow

If user selects "Modify":
1. Prompt: "Describe what changes you'd like to the plan."
2. End turn, wait for user input.
3. If user provides empty/unclear input or cancels ("nevermind"), re-present gate options.
4. Otherwise, update the proposed plan (still markdown, no tasks) and re-present gate.
