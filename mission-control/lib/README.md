# Mission Control Library

Sub-phase documentation for mission-control skill.

## Directory Structure

```
lib/
├── README.md          ← You are here
├── setup/             ← Initialize and plan work
├── preflight/         ← Go/no-go checks before launch
├── execution/         ← Launch agents, collect results
└── control/           ← Handle failures, decide next steps
```

## Conventions

### HIL_GATE_* (Human-In-the-Loop Gates)

Files named `HIL_GATE_*.md` are **pure gates**:

- Present options to the user
- Return the user's decision
- **No side effects** (no task creation, no status changes, no file writes)

This separation enables:
- **Automation**: `--auto` mode skips the gate, not the side effects
- **Testability**: Gates are pure functions of (state → decision)
- **Composability**: Same gate can be reused with different handlers

**Pattern:**
```
PROPOSE_PHASE → HIL_GATE_* → EXECUTE_PHASE
     │              │              │
  prepare       ask user      act on decision
  data/plan     (skippable)   (always runs)
```

**Example (setup phase):**
```
DECOMPOSE → HIL_GATE_PLAN_APPROVAL → MATERIALIZE
    │               │                    │
 propose plan    "approve?"         create tasks
 (markdown)      (pure gate)        (side effects)
```

### HIL_* (Legacy)

Files named `HIL_*.md` (without GATE) currently mix gate + handlers. These are candidates for refactoring to the `HIL_GATE_*` pattern:

- `preflight/HIL_HOLD.md`
- `control/HIL_ANOMALY.md`
- `control/HIL_NEXT_ACTION.md`

Until refactored, these work but aren't cleanly skippable in automation scenarios.

## Phase Files

Each directory contains:
- `PHASE.md` - Overview and flow diagram
- Sub-phase `.md` files - Detailed instructions for each step
