---
name: unwind
description: Discard conversation frames and replace them with a synthetic summary of their side effects. Memoization for context.
---

# /unwind

Let S = S₁ … Sₙ be the conversation. Discard Sⱼ … Sₙ and
produce S\* — a synthetic frame capturing the side effects
of the discarded work.

S\* is the memoized return value. The computation happened,
the results are real, but the trace is replaced by its
output.

Write S\* so that a reader with only S₁ … Sⱼ₋₁ can continue.
