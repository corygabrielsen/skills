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

## The Self-Evident Criterion

Every finding demands improvement. No exceptions.

- **Real bug** → fix
- **False positive** → the artifact was unclear; clarify until intent is obvious
- **Design tradeoff** → document the rationale explicitly

There is no "dismiss." If a reviewer misunderstood, another will too.
