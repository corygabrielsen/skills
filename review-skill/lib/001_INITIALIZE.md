# Initialize

**Parse args, validate target.**

## Do:
- Accept target skill file path from args
- Parse flags (see below)
- Validate file exists with basename `SKILL.md`
- Read the full file content for reviewer prompts
- Store `target_file` path in working memory
- Initialize pass counter to 1

## Don't:
- Start without a target file
- Review non-skill files

## Args:
- First positional arg: path to SKILL.md (required)
- `--auto`: Skip HIL checkpoints (edits proceed without approval)
- `-n N`: N passes where N is a positive integer (default: 1)
- `--reviewer <category>`: Run only `correctness`, `clarity`, or `conformance` reviewers. Reject individual reviewer names (e.g., `adversarial`).

**If validation fails:** Report error and end skill.
