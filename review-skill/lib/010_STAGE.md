# Stage

**Review what will be committed.**

## Do:
- Run `git status {target_file}` to see changes to the target document
- Run `git diff {target_file}` to review staged/unstaged changes
- Stage only the target file using `git add {target_file}` (this skill only edits the target document)

## Don't:
- Stage unrelated changes
- Stage secrets or credentials (.env, *.pem, etc.)
- Use `git add -A` without reviewing first

Proceed to Commit.
