# Fan Out

**Launch all reviewers in parallel. Each reviewer gets a specialized prompt.**

## Do:
- Use `Task` tool with `run_in_background: true` and `prompt: <reviewer prompt>`
- **Substitute `{target_file}`** in each reviewer prompt with the actual target file path before passing to Task (literally replace ALL instances of the string `{target_file}` in the prompt text with the path—leave no placeholders unreplaced)
- Launch all 8 reviewers in a **single assistant turn** (one message containing 8 parallel Task tool calls)
- Launch in this exact order: execution, contradictions, coverage, adversarial, terminology, conciseness, checklist, portability. (Rationale: Tool results are returned in the same order as tool calls, so position-based tracking relies on consistent launch order. Order groups by category: correctness, clarity, conformance.)
- Store all 8 task IDs (from tool response) for collection. Track reviewer by position: (1) execution, (2) contradictions, (3) coverage, (4) adversarial, (5) terminology, (6) conciseness, (7) checklist, (8) portability
- Verify 8 task IDs were returned; if fewer, the result at that position contains an error message instead of a task ID—record "Reviewer [name] failed to launch: [error]" as an issue in the tracker (use "-" for Line column). The skill proceeds with available reviewers. (Note: When a launch failure is recorded, the tracker contains at least one issue, so Collect's condition "ALL 8 reviewers output NO ISSUES AND no launch failures" is false—the skill takes the else branch.)
- Continue to Collect phase with the reviewers that did launch successfully.

## Don't:
- Run reviewers sequentially
- Combine multiple reviewers into one prompt
- Use identical prompts (each reviewer is specialized)

## Reviewer Prompts

**Correctness:**
@lib/prompts/REVIEWER_EXECUTION.md
@lib/prompts/REVIEWER_CONTRADICTIONS.md
@lib/prompts/REVIEWER_COVERAGE.md

**Clarity:**
@lib/prompts/REVIEWER_ADVERSARIAL.md
@lib/prompts/REVIEWER_TERMINOLOGY.md
@lib/prompts/REVIEWER_CONCISENESS.md

**Conformance:**
@lib/prompts/REVIEWER_CHECKLIST.md
@lib/prompts/REVIEWER_PORTABILITY.md
