# Fifth-Generation Programming Notes

## The Generational Shift

| Generation | Artifact | Abstraction |
|------------|----------|-------------|
| 1GL | Machine code | Hardware instructions |
| 2GL | Assembly | Mnemonics |
| 3GL | C, Python | Procedural/OO |
| 4GL | SQL, MATLAB | Declarative, domain-specific |
| **5GL** | **Natural language** | **Intent specification** |

**Key insight**: The prompt becomes the source of truth. Generated code is an artifact — like protobuf bindings from a `.proto` file. You version-control the intent, not the output.

## Fixed Point Theory

The core loop pattern:

```
Reviewer → Addresser → Loop until fixed point
```

Works for any LLM-readable artifact:

| Skill | Artifact | Reviewer | Fixed Point |
|-------|----------|----------|-------------|
| loop-codex-review | Code | Codex CLI | n clean at xhigh |
| loop-address-pr-feedback | Code | Humans + bots | All threads resolved |
| review-skill-parallel | Skill docs | Claude agents | n clean reviews |

**Fixed point** = no reviewer can find anything to flag. Not because you argued them down, but because the artifact is both *correct* AND *self-evident*.

## The Linting Parallel

| 3GL Linting | 5GL Review |
|-------------|------------|
| Deterministic | Stochastic |
| Converges to "no warnings" | Converges to "no findings" |
| Fix issue → issue gone | Fix issue → E[findings] ↓ |

**Why stochastic convergence works**: Different reviewers catch different issues through execution diversity. If all n independent samples return clean, the probability of lurking issues is low.

**Non-monotonic is fine**: Findings may fluctuate (68 → 55 → 62 → 40). Track the trend, not individual counts. The criterion is "expected findings approaches zero."

## The Oscillation Trap

Multiple reviewers can chase each other in circles:

```
Fix conciseness → remove text → create ambiguity
Fix adversarial → add clarifying text → create verbosity
Fix correctness → add edge case handling → more text
→ Back to conciseness...
```

**The escape**: Aggressive consolidation. Don't add clarifying text—rewrite to be both shorter AND clearer. Less surface area = fewer things to flag = stability.

## Convergence Data

`review-skill` fixed-point run (Jan 2026, 3 reviewers: adversarial/conciseness/correctness):

| Iteration | Issues | Pattern |
|:---------:|:------:|:--------|
| 3-7 | 4-18 | Oscillating |
| 8 | 29 | Peak |
| 9-13 | 0 | Stable |

**Breakthrough at iteration 8→9**: Consolidated Core Philosophy from 7 lines to 1. The cycle broke because there was nothing left to cut or clarify.

## The Nash Equilibrium

The fixed point is where three competing pressures balance:

```
           Correctness
           (complete)
              ▲
             /|\
            / | \
           /  ●  \  ← Fixed point
          /   |   \
         ▼────┴────▼
   Conciseness    Adversarial
    (minimal)    (unambiguous)
```

Movement in any direction makes something worse. The document can't be shorter without losing correctness, can't be longer without triggering conciseness, can't be reworded without creating ambiguity.

## The Noise Floor

Simple reviewers converge; deep reviewers always find something.

| Reviewer Depth | Result at Fixed Point |
|----------------|----------------------|
| Simple/fast | NO ISSUES |
| Deep/thorough | ~13 borderline issues |

The "borderline issues" are stable across runs—same findings, not oscillating. They represent the noise floor: real but diminishing-returns improvements.

**Practical criterion**: Simple reviewers return clean. Deep analysis is for auditing, not iteration.

## The Self-Evident Criterion

Every finding demands improvement. No exceptions.

- **Real bug** → fix
- **False positive** → the artifact was unclear; clarify until intent is obvious
- **Design tradeoff** → document the rationale explicitly

There is no "dismiss." If a reviewer misunderstood, another will too.
