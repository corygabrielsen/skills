# Initialize

**Parse args and validate target.**

## Do:
- Accept target skill file path from args
- Parse flags (see below)
- Validate file exists and has basename `SKILL.md`
- Read the full file content for reviewer prompts
- Store `target_file` path in working memory
- Initialize pass counter to 1

## Don't:
- Start without a target file
- Review non-skill files

## Args:
- First positional arg: path to SKILL.md (required)
- `--auto`: Skip HIL checkpoints. Edits proceed without user approval.
- `-n N`: Number of passes (default: 1). Reject non-positive or non-integer values.
- `--reviewer <category>`: Run only reviewers from one category: `correctness`, `clarity`, or `conformance`. Individual reviewer names (e.g., `adversarial`) are not valid. Reject invalid values.

**If validation fails:** Report error and end skill.
