# Setup Phase

**Initialize mission state and plan work.**

## Sub-phases

@INITIALIZE.md
@BOOTSTRAP.md
@DECOMPOSE.md
@HIL_PLAN_APPROVAL.md

## Flow

```
    INITIALIZE
        │
        ├── tasks exist? ──► exit to appropriate phase
        │
        ├── history exists? ──► BOOTSTRAP ─────┐
        │                                      │
        └── fresh start ─────► DECOMPOSE ◄─────┤
                                    │          │
                                    ▼          │
                            HIL_PLAN_APPROVAL  │
                               │    │    │     │
                           approve modify abort│
                               │    │    │     │
                               │    │    ▼     │
                               │    │   END    │
                               │    │          │
                               │    └──────────┘
                               │
                               ▼
                              EXIT
                               │
                               ▼
                        preflight/PHASE
```

Note: BOOTSTRAP also proceeds to HIL_PLAN_APPROVAL (via DECOMPOSE path or directly).

## Entry Conditions
- Skill invoked with `/mission-control`
- May have existing tasks or fresh start

## Exit Conditions
- Plan approved → proceed to preflight/PHASE
- User aborts → end skill
- Tasks already in progress → skip to execution or control
