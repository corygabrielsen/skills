# Fan Out

**Launch reviewers in parallel with specialized prompts.**

If `--reviewer <category>` was specified, launch only reviewers from that category. Otherwise launch all 8.

## Do:
- Use `Task` tool with `run_in_background: true` and `prompt: <reviewer prompt>`
- **Substitute `{target_file}` with the actual file path** in each reviewer prompt (all instances are placeholders)
- Launch reviewers in a **single assistant turn** (one message containing parallel Task tool calls)
- Launch in this order (filtered by `--reviewer` if specified): execution, contradictions, coverage, adversarial, terminology, conciseness, checklist, portability.
- Store task IDs from the Task tool responses. Each Task call returns a task ID string. Store IDs in launch order to match TaskOutput results back to reviewer names.
- Verify expected task IDs were returned. If fewer, the result at that index in the parallel call sequence contains an error message instead of a task ID—store the failure info (Reviewer=[name], error message) for Collect to add to the tracker. Launch failures are infrastructure errors, not document issues; they trigger Synthesize for visibility but skip Triage.
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
