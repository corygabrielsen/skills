# Preflight Phase

**Go/no-go checks before launching agents.**

NASA's Flight Director polls every station before critical operations. One NO-GO triggers a hold.

## Sub-phases

@EVALUATE.md
@HIL_HOLD.md
@FIX.md

## Flow

```
         ┌──────────────────────────────────┐
         │                                  │
         ▼                                  │
     EVALUATE                               │
         │                                  │
    ┌────┴────┐                             │
    │         │                             │
 all GO    any NO-GO                        │
    │         │                             │
    │         ▼                             │
    │     HIL_HOLD                          │
    │     │   │   │   │                     │
    │   fix waive scrub halt                │
    │     │   │     │    │                  │
    │     ▼   │     │    │                  │
    │    FIX  │     │    │                  │
    │     │   │     │    │                  │
    │     └───┼─────┼────┼──────────────────┘
    │         │     │    │
    ▼         ▼     ▼    ▼
  EXIT      EXIT  EXIT  setup/DECOMPOSE
    │         │     │
    ▼         ▼     ▼
 execution/DELEGATE
```

## Entry Conditions
- Tasks exist in pending state with empty blockedBy
- Coming from setup/HIL_PLAN_APPROVAL or control/HIL_NEXT_ACTION (continue)

## Exit Conditions
- All ready tasks are GO → proceed to execution/DELEGATE
- All ready tasks scrubbed → proceed to control/REPORT
- User selects Halt → return to setup/DECOMPOSE
