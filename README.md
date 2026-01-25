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

## Skill Authoring Patterns

Wisdom distilled from iterating on these skills.

### Structure

```
skill-name/
├── SKILL.md              # Main skill document
└── lib/                  # Modular phases (optional)
    ├── 001_INITIALIZE.md
    ├── 002_FAN_OUT.md
    ├── 006_HIL_PLAN_APPROVAL.md   # HIL = human-in-the-loop
    └── ...
```

**Numbered prefixes** (`001_`, `002_`) enforce phase ordering. Use `@lib/...` references in `SKILL.md` to include phases inline.

### Phase Anatomy

Each phase file follows this template:

```markdown
# Phase Name

**One-liner purpose statement.**

## Do:
- Action 1
- Action 2

## Don't:
- Anti-pattern 1
- Anti-pattern 2

## Options (for HIL phases)
AskUserQuestion(...)

## Handlers
What to do for each user choice.
```

### HIL Gates

**Human-in-the-loop checkpoints** at irreversible or high-impact decisions:

| Gate | Purpose |
|------|---------|
| Plan Approval | User approves proposed changes before edits |
| Change Confirmation | User confirms edits before commit |
| Strategy Selection | User chooses between approaches |

Support `--auto` flag to skip HIL (still display plan/summary).

### Core Philosophy

**Every finding demands a change.** No dismiss, no wontfix.

| Finding type | Resolution |
|--------------|------------|
| Real issue | Fix it |
| False positive | Add clarifying text (the code/doc was unclear) |
| Design tradeoff | Document the rationale |

If a reviewer misunderstands, the artifact is unclear. Clarify until misunderstanding is impossible.

### State Schema

Track state in YAML for compaction survival:

```yaml
iteration_count: 0
max_iterations: 10
target: "<path>"
history:
  - iteration: 1
    total_issues: 12
    by_reviewer: { ... }
```

### Flags Convention

| Flag | Behavior |
|------|----------|
| `--auto` | Skip HIL checkpoints (still display summaries) |
| `-n N` | Number of iterations/passes |
| `--reviewer <category>` | Filter to specific reviewer category |

### Quick Reference Table

Every skill should have a phase → purpose table:

```markdown
| Phase | Purpose |
|-------|---------|
| Initialize | Parse args, validate target |
| Fan Out | Launch reviewers in parallel |
| HIL: Plan Approval | User approves proposed fixes |
| ...   | ... |
```

### Composition

Skills can compose other skills:

```
/loop-review-skill-until-fixed-point
    └── calls /review-skill --auto repeatedly
```

The outer skill handles iteration logic; the inner skill handles single-pass execution.
