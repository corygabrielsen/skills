---
name: prove
description: Produce a formal symbolic proof that something is correct, optimal, or complete. Traces all paths, names all invariants, ends with QED or a counterexample.
---

# Prove

You have a claim. Prove it or disprove it. No hand-waving,
no "should work," no "I think this is right." Symbols and
logic until QED or counterexample.

## What Counts as a Proof

A proof traces **every** path through the system and shows
the desired property holds on each one. The structure depends
on the domain:

- **State machines**: Enumerate all states and transitions.
  Show the invariant holds at every step.
- **Algorithms**: Loop invariants, termination arguments,
  induction on input structure.
- **Exhaustiveness**: Case split. Cover every case. Show
  none were missed.
- **Optimality**: Show the bound is tight — exhibit a
  witness for the lower bound.
- **Correctness of code**: Map code to formal semantics.
  Trace concrete values through each branch.

Use symbolic notation naturally — set membership, logical
connectives, quantifiers, implications. Include code
snippets as evidence where they ground the symbols to
reality.

## On Activation

1. **State the claim** precisely. Ambiguous claims can't be
   proved — sharpen first.
2. **Define the system** — what are the states, transitions,
   inputs, invariants? Cite source code with line numbers.
3. **Prove or disprove** — work through every case. If you
   find a counterexample, stop and present it. A clean
   disproof is more valuable than a forced proof.
4. **QED or counterexample** — end with exactly one.

## Anti-patterns

- **Proof by "clearly"** — if it were clear, you wouldn't
  need a proof. Show the step.
- **Proof by exhaustion of patience** — listing some cases
  is not listing all cases.
- **Proof by "the code handles it"** — show HOW the code
  handles it. Trace the values.
- **Ignoring edge cases** — empty inputs, boundary values,
  race conditions, the path nobody takes. These are where
  bugs live.
- **Proving what you want to be true** — if the proof feels
  too easy, you missed a case. Actively try to break it.

---

The user has a claim. Prove or disprove it.
