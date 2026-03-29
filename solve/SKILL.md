---
name: solve
description: Think deeply about a problem, find the right solution, then prove it correct. Rigor over speed.
---

# Solve

Slow down. The first idea is rarely the best idea, and an
unverified idea is worthless. Think, solve, then prove.

## The Shape

1. **Understand** — What is the real problem? Not the
   symptom, not the first framing. Find the essential
   difficulty. Name the constraints, the invariants, the
   failure modes.

2. **Solve** — Find the solution. Explore the space
   honestly — if the obvious approach has a flaw, name it
   and move on. The right solution often isn't the first
   one; it's the one that survives scrutiny.

3. **Prove** — Show it works. Trace every path. Name every
   invariant. End with QED or a counterexample. If you find
   a counterexample, return to step 2 — a disproved solution
   is not a solution. (Invoke `/prove` mentally or literally.)

4. **Summarize** — Prose for humans. What's the solution,
   why is it right, what did you rule out and why.

## On Activation

Work through all four steps. Do not skip the proof — it's
the point. Present your work: the reasoning, the formal
argument, the summary.

If the proof fails, that's the skill working. Iterate until
the solution and proof are consistent, or report that the
problem is harder than it looked.

## Anti-patterns

- **Solving without understanding** — you'll solve the
  wrong problem.
- **Skipping the proof** — "it works" without verification
  is hope, not engineering.
- **Proving the easy part** — prove the part you're least
  sure about, not the part that's obviously correct.
- **One iteration** — if your first solution passes the
  proof on the first try, you probably didn't probe hard
  enough. Actively try to break it.

---

The user has a problem. Understand it, solve it, prove it,
summarize it.
