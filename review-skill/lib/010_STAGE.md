# Stage

**Review what will be committed.**

## Do:
- Run `git diff {target_file}` to review unstaged changes (before staging)
- Run `git status {target_file}` to see overall state
- Stage target file: `git add "{target_file}"` (quote paths)
- If `git add` fails, report error and end

## Don't:
- Stage unrelated changes
- Stage secrets or credentials (.env, *.pem, etc.)
- Use `git add -A`

Proceed to Commit.
