---
name: review-skill
description: Review a skill document using specialized reviewers. Each reviewer finds specific issue types.
---

# Review Skill

Review skill documents using specialized reviewers. Each reviewer is tuned to find specific issue types with high signal and low noise.

## Core Philosophy

**Every issue demands a document change. No exceptions.**

When a reviewer flags something, the document changes. Always. Either:
- **Real issue** → fix the document
- **False positive** → the document was unclear; add clarifying text until the intent is obvious

There is no "dismiss," no "already documented," no "tool behavior." If a reviewer misunderstood, that's a signal the document isn't self-evident—another LLM would misunderstand too. The document must become clearer.

**Fixed point** = no reviewer can find *anything* to flag. Not because you argued them down, but because the document is both **correct** AND **unambiguous**.

---

## Reviewers

Each reviewer asks a focused question. An issue from any reviewer is signal.

**Correctness** — will this do the wrong thing?
| Reviewer | Question | Finds |
|----------|----------|-------|
| execution | "Would this cause wrong behavior?" | Logic errors, missing steps, broken flows |
| contradictions | "Does A contradict B?" | Conflicting instructions |
| coverage | "Is every option/branch handled?" | Unhandled branches, missing handlers |

**Clarity** — will this be misunderstood?
| Reviewer | Question | Finds |
|----------|----------|-------|
| adversarial | "Where would a reasonable LLM go wrong?" | Fixable ambiguities, missing info |
| terminology | "Is term X used consistently?" | Naming inconsistencies |
| conciseness | "Is everything here necessary?" | Verbosity, redundancy, over-explanation |

**Conformance** — does this follow conventions?
| Reviewer | Question | Finds |
|----------|----------|-------|
| checklist | "Do these specific checks pass?" | Structural issues, missing sections |
| portability | "Would this break on non-Claude models?" | Provider-specific assumptions |

---

## Phases

Follow each phase in sequence:

@lib/001_INITIALIZE.md
@lib/002_FAN_OUT.md
@lib/003_COLLECT.md
@lib/004_SYNTHESIZE.md
@lib/005_TRIAGE.md
@lib/006_HIL_PLAN_APPROVAL.md
@lib/007_ADDRESS.md
@lib/008_VERIFY.md
@lib/009_HIL_CHANGE_CONFIRMATION.md
@lib/010_STAGE.md
@lib/011_COMMIT.md
@lib/012_LOOP_GATE.md
@lib/013_EPILOGUE.md

---

## Quick Reference

| Phase | Purpose |
|-------|---------|
| Initialize | Parse args, validate target |
| Fan Out | Launch all reviewers in parallel |
| Collect | Gather and merge results |
| Synthesize | Group by root cause |
| Triage | Propose fixes |
| HIL (Human In the Loop): Plan Approval | Waits for user approval (skipped with `--auto`) |
| Address | Make edits |
| Verify | Confirm changes |
| HIL: Change Confirmation | Waits for user confirmation (skipped with `--auto`) |
| Stage | Review and stage changes |
| Commit | Create commit with proper message |
| Loop Gate | Check pass count, loop or exit |
| Epilogue | Report and end |

## Flags

| Flag | Behavior |
|------|----------|
| `--auto` | Skip HIL checkpoints |
| `-n N` | Do N passes (default: 1) |

---

Parse args for target file (including `-n` flag for pass count, default 1). Launch all 8 reviewers in parallel. Collect results. If all 8 reviewers return NO ISSUES and no launch failures (reviewers that failed to start), skip to Epilogue. Otherwise: Synthesize → Triage → HIL: Plan Approval → Address → Verify → HIL: Change Confirmation → Stage → Commit → Loop Gate. At Loop Gate: if pass < N, re-read target file and loop to Fan Out; else continue to Epilogue.
