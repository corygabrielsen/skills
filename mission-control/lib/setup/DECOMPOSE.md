# Decompose

**Break user request into discrete, delegatable tasks.**

## Do:
- Break work into discrete, independently-completable tasks via `TaskCreate`
- Keep tasks small enough for one agent to complete
- Write descriptions detailed enough that a fresh agent (or mission control post-compaction) can execute without extra context
- Include in each description:
  - What to do (specific, concrete)
  - What files/locations to work with
  - What "done" looks like (verification criteria)
  - Any relevant decisions or constraints from conversation
- Set up dependencies with `TaskUpdate` + `addBlockedBy`

## Don't:
- Create tasks too large or vague for a single agent
- Rely on context that won't survive compaction
- Skip dependency setup when tasks have ordering requirements
- Create tasks you plan to do yourself---all work is delegated
- Create circular dependencies (A blockedBy B, B blockedBy A)---check before adding `addBlockedBy`

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

If the user's request is too vague to decompose into concrete tasks (e.g., "just help me" with no context), ask for clarification before creating tasks. Do not proceed to HIL_PLAN_APPROVAL with an empty task graph.

---

After decomposition, proceed to HIL_PLAN_APPROVAL for user approval.
