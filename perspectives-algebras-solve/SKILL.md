---
name: perspectives-algebras-solve
description: Widen, model, then prove. Runs /perspectives → /algebras → /solve in sequence, each stage feeding the next — multi-stakeholder framing, then a survey of candidate algebras that could model the problem, then a proof against the chosen algebra's invariants.
---

# /perspectives-algebras-solve

Three stages in order. Carry each stage's output into the next.

1. **`/perspectives`** — Frame the problem from all six viewpoints (User non/semi-technical, User technical, Maintainer, Computer Scientist, Mathematician, Agent). Surface what a single stakeholder's framing would miss before committing to a solution.

2. **`/algebras`** — Survey the space of reasonable algebras that could model the problem. Sketch each candidate, name its carriers and operations, evaluate fit against the perspectives from stage 1, and form an opinion on which one to commit to. The chosen algebra defines the invariants stage 3 binds its proof to.

3. **`/solve`** — Understand → solve → prove → summarize, against the invariants from the chosen algebra. End with QED or a counterexample. On a counterexample, return to stage 2 (wrong algebra, or right algebra with a missed invariant) or stage 1 (a missed perspective), then re-descend.

The order is load-bearing: perspectives keeps you from solving the wrong problem, algebras gives you the right structural lens before solving, solve supplies the rigor. Skipping the first two stages is the first-idea trap `/solve` exists to avoid.
