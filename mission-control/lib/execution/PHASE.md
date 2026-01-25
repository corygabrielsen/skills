# Execution Phase

**Launch agents and collect results.**

## Sub-phases

@DELEGATE.md
@MONITOR.md
@VERIFY.md

## Flow

```
    DELEGATE
        │
        ├── --fg mode: block on all agents
        │
        ├── --bg mode: return control to human
        │                   │
        │                   ▼
        │               [human]
        │                   │
        │                   ▼ (resume)
        │
        ▼
    MONITOR
        │
        ▼
    VERIFY
        │
    ┌───┴───┐
    │       │
  pass    fail
    │       │
    ▼       ▼
  EXIT   control/HIL_ANOMALY
    │
    ▼
 control/CHECKPOINT
```

## Entry Conditions
- All ready tasks passed preflight (GO status)
- Coming from preflight/PHASE

## Exit Conditions
- All agents completed and verified → proceed to control/CHECKPOINT
- Verification failed → proceed to control/HIL_ANOMALY
- --bg mode: return control to human after DELEGATE
