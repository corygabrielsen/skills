---
name: fork
description: Branch off a conversation to handle tangents. Outputs context summary and ready-to-paste command for a new terminal session.
---

# Fork Conversation

Generate a context handoff for a new Claude session. No tool calls needed - synthesize from memory and output immediately.

## Output Format

```
## Fork Ready

**Context Summary:**
- Working on: [one sentence - current task]
- Files: [comma-separated paths, max 5-7, mention "+ N others" if more]
- State: [key decisions, branch, blockers, test status - bullets if multiple]

**Paste in new terminal:**
```bash
cd [target directory] && claude "[/skill if needed] [escaped prompt here]"
```

*This conversation continues here. The fork runs independently.*
```

## Building the Command

Generate a shell command with this prompt structure:

```
[/skill-name if specified] Context from forked conversation:
- Working on: [description]
- Files: [list]
- Key context: [important decisions, findings, state]

Continue with: [focus hint from args, or "Pick up where we left off"]
```

If the forked task benefits from a specific skill (e.g., `/mission-control` for coordination, `/plan` for design), prefix the prompt with that skill invocation.

## Args Handling

- `/fork` - generic continuation
- `/fork debug the auth issue` - args become the "Continue with:" line
- `/fork /mission-control coordinate the refactor` - if args start with a skill name, the forked session opens in that skill

## Shell Escaping

- Wrap entire prompt in double quotes
- Escape internal double quotes as `\"`
- Escape dollar signs as `\$`
- Escape backticks as `` \` ``

## What to Include

- Target directory for the forked task (may be current cwd, or different if the tangent involves another repo/location)
- Current task (one sentence)
- Key decisions/findings (not speculation)
- Relevant file paths
- Important state (branch, test status, blockers)

## What NOT to Include

- Full conversation history
- Implementation details already in files
- Secrets or credentials
- Plans not yet acted on
- Tool call history

## Edge Cases

- **Conversation just started**: Minimal fork, note limited context
- **Many files**: Top 5-7 most relevant, note "and N others"
- **Sensitive content**: Omit credentials, note they exist separately

## Anti-patterns

- Multi-paragraph summaries
- Waiting for confirmation
- Suggesting alternatives
- Including noise from tool outputs
