# Decompose

**Break user request into discrete, delegatable tasks (proposed, not created).**

## Do:

- Break work into discrete, independently-completable tasks
- Keep tasks small enough for one agent to complete
- **Propose** tasks in markdown format (do NOT call TaskCreate yet)
- Write descriptions detailed enough that a fresh agent (or mission control post-compaction) can execute without extra context
- Include in each description:
  - What to do (specific, concrete)
  - What files/locations to work with
  - What "done" looks like (verification criteria)
  - Any relevant decisions or constraints from conversation
- Note dependencies (will be set up in MATERIALIZE after approval)

## Don't:

- Call TaskCreate (that's MATERIALIZE's job, after approval)
- Propose tasks too large or vague for a single agent
- Rely on context that won't survive compaction
- Skip dependency notation when tasks have ordering requirements
- Propose circular dependencies (A blocked by B, B blocked by A)

## Task Size Heuristic

A task is the right size if an agent can:
- Understand it from the description alone
- Complete it in one focused session
- Produce a verifiable result

If a task requires multiple unrelated outputs or has internal sequencing, split it.

## Dependency Graph

Visualize as DAG. Parallelize all independent paths.

```
T-001 ──┬── T-003 ──── T-005
        │
T-002 ──┴── T-004
```

Tasks T-001 and T-002 run in parallel. T-003 and T-004 wait for their blockers.

## Empty Request Handling

If the user's request is too vague to decompose into concrete tasks (e.g., "just help me" with no context), ask for clarification before proposing. Do not proceed to HIL_GATE_PLAN_APPROVAL with an empty proposed graph.

## Output Format

```markdown
## Proposed Task Graph

| # | Subject | Description | Blocked By |
|---|---------|-------------|------------|
| 1 | [subject] | [what needs doing] | --- |
| 2 | [subject] | [depends on 1] | 1 |

Execution plan:
1. Task 1 (no blockers)
2. Task 2 (after 1 completes)
```

---

After proposing, proceed to HIL_GATE_PLAN_APPROVAL for user approval.
