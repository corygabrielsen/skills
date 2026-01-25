# Fan Out

**Launch all reviewers in parallel. Each reviewer gets a specialized prompt.**

## Do:
- Use `Task` tool with `run_in_background: true` and `prompt: <reviewer prompt>`
- **Substitute `{target_file}`** in each reviewer prompt with the actual target file path before passing to Task
- Launch all 7 reviewers in a **single assistant turn** (one message containing 7 parallel Task tool calls)
- Launch in this exact order: execution, checklist, contradictions, terminology, adversarial, coverage, portability (position-based tracking depends on this order)
- Store all 7 task IDs (from tool response) for collection—tool results are returned in the same order as tool calls, so track reviewer by position: (1) execution, (2) checklist, (3) contradictions, (4) terminology, (5) adversarial, (6) coverage, (7) portability
- Verify 7 task IDs were returned; if fewer, the result at that position contains an error message instead of a task ID—record "Reviewer [name] failed to launch: [error]" as an issue in the tracker (use "-" for Line column). The skill proceeds with available reviewers. (Note: Launch failures become issues in the tracker, so the skill always takes the "else" branch in Collect—never the no-issues path.)
- Continue to Collect phase with the reviewers that did launch successfully.

## Don't:
- Run reviewers sequentially
- Combine multiple reviewers into one prompt
- Use identical prompts (each reviewer is specialized)

## Reviewer Prompts

@lib/prompts/REVIEWER_EXECUTION.md
@lib/prompts/REVIEWER_CHECKLIST.md
@lib/prompts/REVIEWER_CONTRADICTIONS.md
@lib/prompts/REVIEWER_TERMINOLOGY.md
@lib/prompts/REVIEWER_ADVERSARIAL.md
@lib/prompts/REVIEWER_COVERAGE.md
@lib/prompts/REVIEWER_PORTABILITY.md
