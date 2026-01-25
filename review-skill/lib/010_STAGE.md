# Stage

**Review what will be committed.**

## Do:
- Run `git diff {target_file}` to review unstaged changes (before staging)
- Run `git status {target_file}` to see overall state
- Stage only the target file using `git add "{target_file}"` (quote paths; this skill only edits the target document)
- If `git add` fails, report the error and end the skill
- Optionally run `git diff --staged {target_file}` to verify what will be committed

## Don't:
- Stage unrelated changes
- Stage secrets or credentials (.env, *.pem, etc.)
- Use `git add -A` without reviewing first

Proceed to Commit.
