# Setup Phase

**Initialize mission state and plan work.**

## Sub-phases

@INITIALIZE.md
@BOOTSTRAP.md
@DECOMPOSE.md
@HIL_GATE_PLAN_APPROVAL.md
@MATERIALIZE.md

## Flow

```
    INITIALIZE
        │
        ├── tasks exist? ──► exit to appropriate phase
        │
        ├── history exists? ──► BOOTSTRAP ──► HIL_GATE_PLAN_APPROVAL
        │                         (propose)            │
        └── fresh start ─────► DECOMPOSE ──────────────┤
                                (propose)              │
                                                ┌──────┴──────┐
                                                │      │      │
                                             approve modify abort
                                                │      │      │
                                                │      │      ▼
                                                │      │     END
                                                │      │
                                                │      └─► re-propose
                                                │
                                                ▼
                                           MATERIALIZE
                                          (create tasks)
                                                │
                                                ▼
                                        preflight/PHASE
```

Both BOOTSTRAP and DECOMPOSE **propose** tasks (no TaskCreate).
HIL_GATE_PLAN_APPROVAL is a pure gate (no side effects).
MATERIALIZE creates tasks only after user approval.

## Entry Conditions
- Skill invoked with `/mission-control`
- May have existing tasks or fresh start

## Exit Conditions
- Plan approved → proceed to preflight/PHASE
- User aborts → end skill
- Tasks already in progress → skip to execution or control
