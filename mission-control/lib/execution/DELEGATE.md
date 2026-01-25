# Delegate

**Launch agents for ready tasks.**

A task is "ready" when: status=`pending` AND `blockedBy` is empty. See RULES.md Definitions.

## Do:
- Identify all ready tasks (no blockers, not in_progress, not completed)
- For each ready task:
  - Update status to `in_progress` via `TaskUpdate`
  - Launch via `Task` with `run_in_background: true`
  - Write a clear, specific prompt with full context
  - **Always use the same model as mission control**---do not downgrade agents
- Launch multiple agents in a **single assistant turn** when tasks are independent
- Record task IDs for monitoring

## Don't:
- Launch tasks sequentially when they could be parallel
- Do work yourself that an agent could do
- Downgrade to cheaper/faster models
- Launch tasks that are blocked

## Mode-Specific Behavior

```
--fg (foreground):
    1. Launch all ready tasks in parallel (Task with run_in_background: true)
       - Task(run_in_background: true) for T1
       - Task(run_in_background: true) for T2
       - Task(run_in_background: true) for T3
    2. After ALL agents launched, block serially on each:
       - TaskOutput(task_id=T1, block=true)
       - TaskOutput(task_id=T2, block=true)
       - TaskOutput(task_id=T3, block=true)
       (Agents run in parallel; waiting is serial—this is optimal)
    3. → execution/VERIFY when all complete (skip MONITOR in --fg mode)

--bg (background):
    1. Launch all ready tasks in parallel (Task with run_in_background: true)
    2. Report launched tasks to user
    3. Return control to human, end skill
    4. On resume (user returns or invokes /mission-control) → INITIALIZE routes to execution/MONITOR
```

**Note:** In --fg mode, DELEGATE handles all blocking. MONITOR phase is skipped (pass-through).

## Agent Launch Syntax

Use the `Task` tool with these parameters:
```
Task(
  prompt: "You are executing task {task_id}: {task_subject}\n\n{task_description}\n\nWhen complete:\n- Report what you accomplished\n- Note any issues encountered\n- Confirm verification criteria are met",
  description: "{brief task summary for logs}",
  subagent_type: "general-purpose",
  run_in_background: true
)
```

**Note:** When launching multiple agents, include all Task calls in a single assistant turn (message) for parallel execution.

The `Task` tool returns an agent ID. Store it in task metadata: `TaskUpdate(taskId: T-001, metadata: {agent_id: "returned_id"})`.

## Launch Failure Handling

If `Task` tool returns an error instead of an agent ID:
1. Record error in task metadata for the failed task
2. Mark that task: `BLOCKED - Launch failed: [error]`
3. **Continue launching remaining tasks in the batch** (don't abort the whole batch)
4. After batch completes: if any launches failed → control/HIL_ANOMALY for failed tasks
5. Successfully launched tasks proceed normally to MONITOR/VERIFY

See FR-A001 in RULES.md for full details.

## Output Format (--bg mode)

```markdown
## Launched Agents

| Task | Agent | Description |
|------|-------|-------------|
| T-002 | agent-1 | [brief] |
| T-004 | agent-2 | [brief] |

Agents running. Notifications may be lost (~50%)—poll with `/mission-control` to check status.
```
