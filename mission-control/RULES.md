# Mission Control Rules

Pre-planned decisions to minimize real-time discussion. The operational bible.

---

## Mission Rules (MR)

Inviolable constraints. No exceptions without explicit user override.

| Rule | Constraint |
|------|------------|
| MR-1 | Task system is source of truth, not context |
| MR-2 | No agent launch without Pre-Flight GO |
| MR-3 | No task marked complete without verification |
| MR-4 | Never delete tasks; use ABORTED status |
| MR-5 | Never downgrade agent model from mission control's model |
| MR-6 | Descriptions must survive context compaction |

---

## Flight Rules (FR)

### Section A: Agent Operations

#### FR-A001: Agent Launch Failure

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Task tool returns error instead of agent ID | Record in task metadata | Launch failures are signal |
| | Mark task: `BLOCKED - Launch failed: [error]` | Preserves error context |
| | → HIL: Anomaly | Human decides retry vs replan |

#### FR-A002: Agent Timeout

| Condition | Threshold | Action | Rationale |
|-----------|-----------|--------|-----------|
| No output received | >5 min (simple task) | Poll via TaskOutput | Notifications unreliable (~50% lost) |
| No output received | >15 min (complex task) | Poll via TaskOutput | Complex work takes longer |
| No output after poll | Still running | Continue waiting | Agent may be mid-execution |
| No output after poll | Task appears stalled | → HIL: Anomaly | Human decides next action |

#### FR-A003: Agent Output Malformed

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Output not parseable | Record raw output in task metadata | Preserve evidence |
| | Do NOT mark complete | Cannot verify success |
| | → HIL: Anomaly | Human must interpret |

---

### Section B: Verification

#### FR-B001: Verification Failure

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Tests fail after agent reports success | Do NOT mark task complete | Agent introduced bugs or misunderstood |
| | Do NOT auto-retry | Same approach may repeat failure |
| | Record failure details | Evidence for diagnosis |
| | → HIL: Anomaly | Human decides: retry, replan, skip |

#### FR-B002: Verification Cannot Run

| Condition | Action | Rationale |
|-----------|--------|-----------|
| No tests exist for task type | Document in task metadata | Acknowledge gap |
| | Apply spot-check verification | Some verification > none |
| | → HIL: Anomaly if uncertain | Human accepts risk or adds verification |

#### FR-B003: Partial Success

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Agent completed some but not all objectives | Do NOT mark complete | Partial is not complete |
| | Create follow-up task for remainder | Preserve what was done |
| | Mark original: `ABORTED - Partial, see [follow-up]` | Clear trail |

---

### Section C: Task Management

#### FR-C001: Task Too Large

| Condition | Indicator | Action | Rationale |
|-----------|-----------|--------|-----------|
| Task scope exceeds single agent capacity | Description >500 words | Flag at Pre-Flight | Large tasks fail more often |
| | Multiple distinct deliverables | Split into subtasks | Each agent needs clear focus |
| | Estimated >30min work | Consider decomposition | Long tasks risk timeout |

#### FR-C002: Task Description Insufficient

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Missing file paths | NO-GO at Pre-Flight | Agent will guess wrong |
| Missing success criteria | NO-GO at Pre-Flight | Cannot verify completion |
| Ambiguous scope | NO-GO at Pre-Flight | Agent may over/under-deliver |
| References context not in description | NO-GO at Pre-Flight | Won't survive compaction |

#### FR-C003: Circular Dependencies

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Task A blockedBy B, B blockedBy A | Reject at Decompose | Deadlock guaranteed |
| | Restructure dependency graph | One must go first |

---

### Section D: Context Management

#### FR-D001: Context Approaching Limit

| Condition | Threshold | Action | Rationale |
|-----------|-----------|--------|-----------|
| Context utilization | >70% | Warning: consider Handoff soon | Compaction loses detail |
| Context utilization | >85% | → Handoff phase | Capture state before forced compaction |

#### FR-D002: Post-Compaction Recovery

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Conversation compacted | Run TaskList immediately | Tasks are source of truth |
| | Read any HANDOFF task | Recover mission state |
| | Do NOT rely on memory | Context is gone |

---

### Section E: Mode-Specific

#### FR-E001: Background Mode Return

| Condition | Action | Rationale |
|-----------|--------|-----------|
| --bg mode, agents launched | Report status table | User needs visibility |
| | List agent IDs and tasks | User can poll manually |
| | Return control to human | That's what --bg means |
| | Do NOT block waiting | User may have other work |

#### FR-E002: Foreground Mode Blocking

| Condition | Action | Rationale |
|-----------|--------|-----------|
| --fg mode, agents launched | Immediately call TaskOutput(block: true) | Maintain control flow |
| | Block on ALL launched agents | Parallel launch, serial wait |
| | Proceed to Verify when all complete | Continue the loop |

#### FR-E003: Auto Mode Boundaries

| Condition | Action | Rationale |
|-----------|--------|-----------|
| --auto mode, all tasks GO | Proceed without HIL | That's what --auto means |
| --auto mode, any NO-GO or failure | Exit --auto, invoke HIL | Human decides all non-nominal cases |
| --auto mode, all tasks complete | Generate summary, end | Mission complete |

---

## Rule Amendments

Rules may only be amended by explicit user instruction during a mission. Document any amendments in task metadata:

```yaml
metadata:
  rule_amendments:
    - rule: FR-B001
      amendment: "User accepted partial verification for this task"
      reason: "Time constraint, will verify manually later"
```

---

## Quick Reference

| Code | Section |
|------|---------|
| MR-* | Mission Rules (inviolable) |
| FR-A* | Agent Operations |
| FR-B* | Verification |
| FR-C* | Task Management |
| FR-D* | Context Management |
| FR-E* | Mode-Specific |
