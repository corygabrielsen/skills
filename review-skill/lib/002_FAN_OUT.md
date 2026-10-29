# Fan Out

**Launch reviewers in parallel with specialized prompts.**

If `--reviewer <category>` was specified, launch only reviewers from that category. Otherwise launch all 8.

## Do:

- Create a fresh directory for this pass: `PASS_DIR=$(mktemp -d "${TMPDIR:-/tmp}/review-skill.XXXXXX")`
- Launch each reviewer as a background subagent named after its reviewer (e.g. `execution`)
- **Substitute `{target_file}` with the actual file path** in each reviewer prompt (all instances are placeholders)
- Append to each prompt: "Write your output, in exactly the format above, to `<PASS_DIR>/<reviewer>.md`. Your final message is `written`."
- Launch reviewers in a **single assistant turn** (one message containing all launches)
- Launch in this order (filtered by `--reviewer` if specified): execution, contradictions, coverage, adversarial, terminology, conciseness, checklist, portability.
- A launch that returns an error instead of starting a reviewer is a launch failure—store (Reviewer=[name], error message) for Collect to add to the tracker. Launch failures are infrastructure errors, not document issues; they trigger Synthesize for visibility but skip Triage.
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
