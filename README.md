# claude-skills

Custom skills for Claude Code.

## Installation

```bash
make install    # Symlink all skills to ~/.claude/skills
make uninstall  # Remove symlinks
make list       # Show installed skills
```

Requires `pre-commit` for hooks:
```bash
pre-commit install --hook-type pre-commit --hook-type commit-msg
```

## Skills

| Skill | Description |
|-------|-------------|
| `/debrief` | Reconstruct context after returning to a conversation |
| `/fork` | Branch off a conversation to handle tangents |
| `/loop-address-pr-feedback` | Address PR review feedback until all threads resolved |
| `/loop-codex-review` | Codex review loop with progressive reasoning levels |
| `/loop-review-skill-parallel` | Iterate skill review until fixed point |
| `/mission-control` | Coordinate multi-step work with task graphs |
| `/next` | Present 2-4 actionable next steps |
| `/review-pr` | Thorough PR review process |
| `/review-skill-parallel` | Single iteration of parallel skill review |
