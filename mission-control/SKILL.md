---
name: mission-control
description: Coordinate multi-step work through delegated agents with a plan that survives context compaction. Use when work splits into several independent units, or when handed a context blob from /fork.
args:
  - name: --auto
    description: Skip plan approval; a FAIL still goes to the human.
---

# Mission Control

Coordinate and verify; agents do the work.

The work is the arguments, else the conversation; from /fork, the "Continue with" line is the work and the rest is context. One unit is a one-task plan.

Missions live in `~/.local/state/mission-control/<cwd-slug>/<mission>/`; resume an open one there before starting another, and print the absolute path when creating one. PLAN.md holds the tasks (scope, files touched, done-when), their dependencies, each task's status (pending, running with agent name, PASS, FAIL), and decisions as they are made. After compaction, reread it before acting.

Get approval on the plan unless `--auto`. Agents run in the background; dispatch each task once its dependencies pass, and keep advancing as completions arrive until the graph completes or what remains waits on the human. Concurrent writers to one repo get separate worktrees unless their files are disjoint, counting shared lockfiles and manifests; merge passing branches back and rerun the done-when checks on the merged tree.

Give each agent the absolute path `results/<task>.md` in the mission dir; first line PASS or FAIL. A finished agent with no result file is a FAIL. Read only the FAILs. A worker's PASS is a claim: confirm it against real state before marking the task PASS. A FAIL holds its dependents and goes to the human; independent tasks continue.

A task citing `[[decision:<memo>#<qid>]]` first runs /drift-scan on that memo and qid; dispatch only if the decision is locked and the verdict is clean. Anything else goes to the human with the report.

When nothing more can run, report and stop for the human's next call.
