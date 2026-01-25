# Initialize

**Parse args and validate target.**

## Do:
- Accept target skill file path from args
- Parse `--auto` flag if present (enables unattended mode)
- Validate file exists and has basename `SKILL.md`
- Read the full file content for reviewer prompts
- Store `target_file` path in working memory for substitution into prompts, commands, and output templates

## Don't:
- Start without a target file
- Review non-skill files

## Args:
- First positional arg: path to SKILL.md (required)
- `--auto`: Skip HIL checkpoints (Plan Approval, Change Confirmation). Edits proceed without user approval.

**If validation fails:** Report error and end skill.
