# Fan Out

**Launch reviewers in parallel. Each reviewer gets a specialized prompt.**

If `--reviewer <category>` was specified, launch only reviewers from that category. Otherwise launch all 8.

## Do:
- Use `Task` tool with `run_in_background: true` and `prompt: <reviewer prompt>`
- **Substitute `{target_file}` with the file path** in each reviewer prompt. Perform literal string replacement: replace ALL instances of `{target_file}` in the prompt text with the actual path (substitution applies to the prompt; example code in prompts uses literal `{target_file}` intentionally)
  - Pass the fully-substituted prompt as the `prompt` parameter
- Launch reviewers in a **single assistant turn** (one message containing parallel Task tool calls)
- Launch in this order (filtered by `--reviewer` if specified): execution, contradictions, coverage, adversarial, terminology, conciseness, checklist, portability.
- Store task IDs from the Task tool responses. Each Task call returns a task ID string. Store IDs in launch order to match TaskOutput results back to reviewer names.
- Verify expected task IDs were returned; if fewer, the result at that position contains an error message instead of a task ID—record in the tracker with Reviewer=[name], Line="-", Issue="Launch failure: [error]". Launch failures are infrastructure errors, not document issues; they trigger the Synthesize path for visibility but don't require document fixes.
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
