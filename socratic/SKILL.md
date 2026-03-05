---
name: socratic
description: Reveal user intent through narrowing binary questions, increasingly specific, with opinionated defaults when asked.
---

# Socratic

The user has a decision to make but can't fully specify it upfront. Reveal their
intent through a sequence of narrowing questions — not by guessing, not by dumping
options, and not by asking open-ended questions that put the burden back on them.

If the user's intent is already clear, act on it — don't manufacture decisions.

## When to Use

- A decision has multiple plausible approaches and you're unsure which the user wants
- The user says something vague ("fix this", "clean this up", "handle the edge cases")
- You're about to propose a solution but realize you're making assumptions

This skill can be invoked explicitly, but also apply it proactively whenever you'd
otherwise guess or present a wall of options.

## The Protocol

### 1. Frame the decision

State what needs deciding in one sentence. Include enough context that the user
doesn't have to remember — they may be returning cold.

### 2. Ask binary questions

Use `AskUserQuestion` with 2 options (occasionally 3 when there's a genuine third
path). Each question should:

- Have **clear phrasing** — the question makes sense without re-reading prior context
- Have **concrete options** — not "option A" vs "option B" but what each does
- Include a brief **description** explaining the tradeoff

(`AskUserQuestion` is a Claude Code tool. In other environments, present options
inline as a numbered list.)

```
Wide:    "Fix it here or leave for follow-up?"
Medium:  "Validate on input or on use?"
Narrow:  "Schema-level constraint or application check?"
```

Stay under 7 questions. If you need more, you're conflating independent decisions —
finish one, then start the next.

### 3. Handle responses

| Response                          | Action                                                                                                    |
| --------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Clean pick                        | Advance to next question or act                                                                           |
| "A, but only if X"                | Fold the caveat in and continue narrowing                                                                 |
| "Neither" / third option          | Reframe — a hidden constraint invalidated your options. Ask what's wrong with both to discover it         |
| "What do you recommend?"          | Take a position. State your opinion clearly, give reasoning (2-3 sentences), let them confirm or redirect |
| "I don't know" / "you decide"     | Make the call, state what you chose and why, proceed without asking for confirmation                      |
| Ambiguous ("yeah sure")           | Clarify which option they're affirming before proceeding                                                  |
| "Go back, change my first answer" | Rewind to that point and re-narrow from there                                                             |

Don't hedge. "I think X because Y" beats "X has these tradeoffs and Y has these
tradeoffs and it depends on..."

### 4. Act on the result

Once intent is clear, execute immediately. Don't summarize the decision tree back.

## Anti-patterns

- **Open-ended first question** — "How do you want to handle this?" puts all the
  cognitive load on the user. Start with a binary.
- **More than 3 options per question** — You haven't narrowed enough. Group into 2
  categories first, then drill down.
- **Re-asking after an answer** — Each answer should advance the state. If you're
  asking the same question with different framing, you didn't listen to the answer.
- **Dumping context before the question** — Lead with the question. Add context in
  the description field, not as paragraphs that bury the choice.

---

The user has a decision to make. Frame it and ask the first binary question.
