# Commit

**Create commit with proper message format.**

## Do:
- Create commit with subject and body following the rules below
- Use the heredoc pattern shown in Command section

## Message Rules

1. Subject ≤42 chars (leaves room for GitHub PR number)
2. Imperative mood ("Add feature" not "Added feature")
3. Capitalize subject, no period at end
4. Blank line between subject and body
5. Body: explain what and why, wrap at 72 chars
6. Use backticks around `filenames`, `paths`, and `code_symbols`

## Command
```bash
git commit -m "$(cat <<'EOF'
<Subject line - imperative, ≤42 chars>

<Body - what and why, wrapped at 72 chars>
Use `backticks` around files, paths, symbols.
EOF
)"
```

## Don't:
- Write vague subjects
- Exceed character limits
- Skip the body for non-trivial changes
- Forget backticks around technical terms

## If commit fails:
- **Pre-commit hook failure**: Fix issue, re-stage if needed, create NEW commit (don't amend—original didn't happen)
- **No staged changes**: Return to Stage phase to diagnose
- **Other git error**: Report the error and end the skill

Proceed to Loop Gate.
