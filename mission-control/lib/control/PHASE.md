# Control Phase

**Handle anomalies, check status, decide next action.**

## Sub-phases

@HIL_ANOMALY.md
@CHECKPOINT.md
@REPORT.md
@HIL_NEXT_ACTION.md
@HANDOFF.md

## Flow

```
                    ┌─────────────────────────────┐
                    │                             │
                    ▼                             │
             HIL_ANOMALY ◄── verification fail    │
                    │                             │
            ┌───────┼───────┐                     │
            │       │       │                     │
         retry   replan   skip                    │
            │       │       │                     │
            │       │       ▼                     │
            │       │   CHECKPOINT                │
            │       │       │                     │
            │       │       ▼                     │
            │       │    REPORT                   │
            │       │       │                     │
            │       │       ▼                     │
            │       │  HIL_NEXT_ACTION            │
            │       │   │    │    │    │          │
            │       │ cont pause add complete     │
            │       │   │    │    │    │          │
            │       │   │    │    │    ▼          │
            │       │   │    │    │   END         │
            │       │   │    │    │               │
            │       │   │    ▼    ▼               │
            │       │   │ HANDOFF setup/DECOMPOSE │
            │       │   │    │                    │
            │       │   │    ▼                    │
            │       │   │   END                   │
            │       │   │                         │
            └───────┼───┼─────────────────────────┘
                    │   │
                    ▼   ▼
              preflight/PHASE
```

## Entry Conditions
- Coming from execution/VERIFY (pass or fail)
- Coming from execution/MONITOR (--bg mode resume)

## Exit Conditions
- Continue → return to preflight/PHASE
- Pause → HANDOFF then end
- Add work → return to setup/DECOMPOSE
- Complete → end skill
- Retry/Replan → return to preflight/PHASE
