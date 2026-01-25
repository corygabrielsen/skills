# Stage

**Review what will be committed.**

## Do:
- Run `git status` to see changes
- Run `git diff` to review staged/unstaged changes
- Stage relevant files (prefer explicit paths over `git add -A`)

## Don't:
- Stage unrelated changes
- Stage secrets or credentials (.env, *.pem, etc.)
- Use `git add -A` without reviewing first
