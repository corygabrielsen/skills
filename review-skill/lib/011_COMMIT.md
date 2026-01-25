# Commit

**Create commit with proper message format.**

## Do:
- Create commit with subject and body following the rules below
- Use the heredoc pattern shown in Command section

## Message Rules

1. Subject ≤42 chars (room for ` (#NNNN)` suffix → 50 char limit)
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
- Write vague subjects ("fix stuff", "updates")
- Exceed character limits (count them!)
- Skip the body for non-trivial changes
- Forget backticks around technical terms

Proceed to Loop Gate.
