# Delegate

**Launch agents for ready tasks.**

A task is "ready" when: status=`pending` AND `blockedBy` is empty.

## Pre-dispatch decision gate (Tier-A guard)

Run BEFORE marking any task `in_progress`. Skips cleanly when no decision citations exist; never gold-plates trivial tasks.

**Decision states** (from the cited memo's `decisions:` frontmatter):

- `locked` — premise verified and committed. Safe to dispatch.
- `contested` — premise challenged (by drift evidence or human review). Block.
- `revised` — superseded by a follow-up decision (see `superseded_by`). Block until task cites the revision.
- _(missing entry)_ — qid not found in the memo. Block as a broken citation.

For each ready task:

1. **Scan task description + linked memo for citations** of the form `[[decision:<memo-slug>#<qid>]]`. If none → skip the gate (Tier-B/C task; proceed to normal launch).
2. **For each cited decision**, read the source memo's frontmatter `decisions:` list:
   - Locate the entry with matching `qid`.
   - If `state: locked` → continue.
   - If `state: contested` → **BLOCK**. Surface to control/HIL_ANOMALY: "Task <id> cites contested decision <slug>#<qid>; revision required before dispatch."
   - If `state: revised` → **BLOCK** until task description updated to cite the revision (`superseded_by`). Surface to human.
   - If entry missing → **BLOCK**. Surface "Task <id> cites <slug>#<qid> but decision not present in memo frontmatter."
3. **Run drift-scan on the union of cited decisions** (one Skill invocation per memo, in parallel):
   - `Skill(skill: "drift-scan", args: "<memo-path>")` — or `--qid <qid>` to narrow.
   - If verdict `clean` → proceed.
   - If verdict `drifted` → **BLOCK**. Surface to control/HIL_ANOMALY with the drift report. A human flips the cited decision's `state: contested` in the memo — the gate does not auto-mutate the source of truth.
   - If verdict `unverifiable` → soft-block; surface to human for adjudication. Do not auto-proceed.

Gate output before launch: one line per task `GATE: <task-id> <pass|blocked-<reason>>`.

## Do:

- Identify all ready tasks (no blockers, not in_progress, not completed)
- **Run the pre-dispatch decision gate** (above). Drop blocked tasks from this batch.
- For each ready task that passed the gate:
  - Update status to `in_progress` via `TaskUpdate`
  - Launch via `Task` with `run_in_background: true`
  - Write a clear, specific prompt with full context
  - **Match mission control's model** (if framework supports model selection)
- Launch multiple agents in a **single assistant turn** when tasks are independent
- Record task IDs for monitoring

## Don't:

- Launch tasks sequentially when they could be parallel
- Do work yourself that an agent could do
- Downgrade to cheaper/faster models (if framework supports model selection)
- Launch tasks that are blocked

## Mode-Specific Behavior

**Note:** `--auto` forces `--fg` behavior even if originally invoked with `--bg`.

```
--fg (foreground), or --auto:
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
2. Mark that task: `ABORTED - Launch failed: [error]`
3. **Continue launching remaining tasks in the batch** (don't abort the whole batch)
4. After batch completes: if any launches failed → control/HIL_ANOMALY for failed tasks
5. Successfully launched tasks proceed normally to MONITOR/VERIFY

See FR-A001 in RULES.md for full details.

## Output Format (--bg mode)

```markdown
## Launched Agents

| Task  | Agent   | Description |
| ----- | ------- | ----------- |
| T-002 | agent-1 | [brief]     |
| T-004 | agent-2 | [brief]     |

Agents running. Notifications may be lost (~50%)—poll with `/mission-control` to check status.
```
