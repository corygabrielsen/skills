---
name: loop-review-skill-until-fixed-point
description: Iterate /review-skill on a target until fixed point. Runs review passes until all reviewers return NO ISSUES.
---

# Loop Review Skill Until Fixed Point

Run `/review-skill` on a target document repeatedly until fixed point—when all reviewers return NO ISSUES.

## Core Concept

```
/review-skill <target> --auto
         │
         ▼
    ┌─────────┐
    │ Issues? │
    └────┬────┘
         │
    yes ─┴─ no
     │      │
     ▼      ▼
  repeat   done (fixed point)
```

Fixed point = the document is both correct AND unambiguous. No reviewer can find anything to flag.

---

## Arguments

| Arg | Required | Description |
|-----|----------|-------------|
| `<target>` | yes | Path to SKILL.md to review |

---

## State

```yaml
max_iterations: 10        # Safety limit
iteration_count: 0        # Current iteration
target: "<from args>"     # Target SKILL.md path
history: []               # Per-iteration metrics for convergence tracking
```

### History Entry Schema

Each iteration appends to `history`:

```yaml
- iteration: 1
  total_issues: 12
  by_reviewer:
    execution: 2
    contradictions: 0
    coverage: 3
    adversarial: 4
    terminology: 1
    conciseness: 2
    checklist: 0
    portability: 0
  converged: [contradictions, checklist, portability]  # reviewers with 0 issues
```

---

## Phase: Initialize

1. Parse target path from arguments
2. Set `iteration_count = 0`
3. Set `max_iterations = 10`
4. Confirm target exists

---

## Phase: Loop

```
while iteration_count < max_iterations:
    iteration_count += 1

    1. Run: /review-skill <target> --auto
    2. Parse result, count issues per reviewer
    3. Record in history: { iteration, total_issues, by_reviewer, converged }
    4. Output iteration summary (see below)
    5. If all reviewers return "NO ISSUES" → FIXED POINT, exit loop
    6. Else → issues were addressed, continue loop
```

### Iteration Summary (output after each iteration)

After each `/review-skill` pass completes, output:

```markdown
### Iteration {N} Summary

| Reviewer | Issues | Δ |
|----------|-------:|--:|
| execution | 2 | -1 |
| contradictions | 0 | ✓ |
| coverage | 3 | +1 |
| adversarial | 4 | -2 |
| terminology | 1 | 0 |
| conciseness | 2 | -3 |
| checklist | 0 | ✓ |
| portability | 0 | ✓ |
| **Total** | **12** | **-5** |

Converged: 3/8 reviewers (contradictions, checklist, portability)
```

- **Δ column**: Change from previous iteration. `✓` = converged (0 issues), `+N`/`-N` = delta, `—` = first iteration
- **Converged**: Reviewers with 0 issues this iteration

### Exit Conditions

| Condition | Action |
|-----------|--------|
| All reviewers return "NO ISSUES" | Fixed point reached, exit with success |
| `iteration_count >= max_iterations` | Safety limit hit, ask user how to proceed |

---

## Phase: Report

Present final state with convergence trend:

```markdown
## Loop Complete

| Metric | Value |
|--------|-------|
| Target | {target} |
| Iterations | {iteration_count} |
| Fixed point reached | yes/no |
| Final state | clean / max iterations hit |

### Convergence Trend

| Iter | Total | exec | cont | covr | advr | term | conc | chkl | port |
|-----:|------:|-----:|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| 1    | 24    | 3    | 2    | 4    | 8    | 2    | 5    | 0    | 0    |
| 2    | 18    | 2    | 1    | 3    | 6    | 1    | 4    | 0    | 1    |
| 3    | 12    | 1    | 0    | 2    | 5    | 1    | 3    | 0    | 0    |
| ...  | ...   | ...  | ...  | ...  | ...  | ...  | ...  | ...  | ...  |
| N    | 0     | 0    | 0    | 0    | 0    | 0    | 0    | 0    | 0    |

(Abbreviated reviewer names: exec=execution, cont=contradictions, covr=coverage,
advr=adversarial, term=terminology, conc=conciseness, chkl=checklist, port=portability)
```

This table shows issue counts decreasing toward fixed point. Non-monotonic behavior (increases) signals oscillation or reviewer variance.

### Convergence Chart (optional)

If `plotext` is available, generate a terminal chart showing convergence curves:

```python
import plotext as plt

# history = list of {iteration, total_issues, by_reviewer} dicts
iterations = [h['iteration'] for h in history]
total = [h['total_issues'] for h in history]
conc = [h['by_reviewer']['conciseness'] for h in history]
advr = [h['by_reviewer']['adversarial'] for h in history]

plt.plot(iterations, total, label="total", marker="braille")
plt.plot(iterations, conc, label="conciseness", marker="braille")
plt.plot(iterations, advr, label="adversarial", marker="braille")
plt.title("Convergence to Fixed Point")
plt.xlabel("Iteration")
plt.ylabel("Issues")
plt.theme("dark")
plt.plotsize(80, 15)
plt.show()
```

Example output:
```
                         Convergence to Fixed Point
    ┌──────────────────────────────────────────────────────────────┐
 24┤⢕⢕ total                                                      │
   │⢕⢕ conciseness                                                │
 18┤⢕⢕ adversarial                                                │
   │⠑⠢⠤⣀     ⠑⢄                                                    │
 12┤     ⠉⠑⠢⠤⣀⣀⣀⣀⣀                                                │
   │              ⠉⠉⠉⠒⠒⠢⠤⠤⡀                                       │
  6┤                      ⠈⠑⠢⣀                                    │
   │⢄⣀⣀                       ⠉⠒⠒⠢⠤⠤⣀                             │
  0┤⠑⠒⠒⠒⠒⠢⠤⠤⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀│
   └┬────────────┬────────────┬────────────┬────────────┬──────────┘
   1            3            5            7            9
Issues                      Iteration
```

The braille markers show smooth decay curves. Reviewers converge at different rates depending on the document's issues.

To check availability: `python3 -c "import plotext" 2>/dev/null && echo "available"`. Skip chart if not installed.

### Sparkline Summary (alternative)

For a compact single-line-per-reviewer view:

```python
SPARKS = " ▁▂▃▄▅▆▇█"

def spark(values, max_val):
    return "".join(SPARKS[min(8, int(v / max_val * 8))] for v in values)

# Example: conciseness over 15 iterations
# [28, 22, 18, 14, 10, 8, 6, 4, 3, 2, 1, 0, 0, 0, 0]
# Output: █▇▆▅▄▃▂▁▁      (converged at iter 12)
```

Example output:
```
Reviewer       Trend              Start  End  Converged @
execution      ▂▁                     2    0            3
contradictions                        0    0            1
coverage       ▄▂▁                    4    0            4
adversarial    ▆▄▂▁                   6    0            5
terminology    ▂▁                     2    0            3
conciseness    █▆▄▂▁                  8    0            6
checklist                             0    0            1
portability                           0    0            1

Total: █▄▂▁           (22 → 0)
```

If fixed point reached:
> {target} has reached a fixed point after {N} iterations.
> The document is now internally consistent and unambiguous.

If max iterations hit:
> Safety limit reached after {max_iterations} iterations.
> The document may still have issues. Consider increasing the limit or investigating.

---

Begin now.
