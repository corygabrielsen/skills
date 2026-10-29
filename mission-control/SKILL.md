---
name: mission-control
description: Coordinate multi-step work through delegated agents with a plan that survives context compaction. Use when work should be delegated to agents, or when handed a context blob from /fork.
args:
  - name: --auto
    description: Skip plan approval only; FAIL and held tasks still go to the human.
---

# Mission Control

Coordinate and verify; agents do the work.

The work is the arguments, else the conversation; from /fork, the "Continue with" line is the work and the rest is context. Single-task work still gets a plan.

Each mission is a directory `~/.local/state/mission-control/<cwd-slug>/<mission>/`, where `<cwd-slug>` is the absolute working directory with `/` replaced by `-` and `<mission>` is a short kebab-case name chosen at creation; print the absolute path when creating one. A mission is open until the human says to close it; then write `closed` as the first line of its PLAN.md. Resume the open mission that the work continues, else start a new one; when several fit or the fit is unclear, ask.

The mission's PLAN.md holds each task (kebab-case id, scope, files touched, done-when, cited decisions), its dependencies, and its status: pending, running (with agent name), held (waiting on a human decision), PASS, or FAIL with the reason. It also records the human's decisions. PLAN.md, not a result file, is the authority on status. After compaction, reread it before acting. Adding, removing, or rescoping tasks or dependencies in an approved plan goes back for approval unless `--auto`.

Get approval on the plan unless `--auto`. Agents run in the background; dispatch each pending task once its dependencies are PASS, and keep going as completions arrive. Concurrent agents writing to one repo get separate worktrees unless their files are disjoint, counting shared lockfiles and manifests.

Give each agent the absolute path of `results/<task-id>.md` in the mission dir, first line PASS or FAIL. Read only the first line of a PASS: it is a claim. Confirm it with the done-when check in the agent's tree; for a worktree, then, once no agent is writing in the working directory, merge its branch into the branch checked out there and rerun the check. The task stays running until then, and nothing new is dispatched into the working directory while a merge waits. A merge conflict aborts the merge and a failed rerun reverts it. A failed check, conflict, or failed rerun is a FAIL; the task is PASS only when the check holds. An agent that finished with no result file, or whose first line is neither PASS nor FAIL, is a FAIL. Read the full result file of an agent's own FAIL.

A FAIL's dependents stay pending; independent tasks continue, and FAILs are reported at the stop. When the human resolves a FAIL or held task, record the decision and set the task to pending (deleting its result file) or remove it from the plan; removing a task needs a decision for each of its dependents too.

Before dispatching a task with a cited `[[decision:<memo>#<qid>]]`, resolve `<memo>` to its file in the repo and run /drift-scan on it and the qid. Dispatch only if the memo records the decision as `locked` and the verdict is `clean`, or the human has recorded a decision to proceed for that task; otherwise hold the task and include the report (or the missing memo) at the stop.

When nothing is running and no pending task can be dispatched, report and stop for the human's next call.
