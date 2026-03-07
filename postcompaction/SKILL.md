---
name: postcompaction
description: Recover rich context after conversation compaction. Dispatches a subagent to read the full transcript and extract nuance the compaction summary missed.
---

# Post-Compaction Context Recovery

Compaction summaries preserve facts but lose nuance. This skill
recovers nuance that compaction drops by reading the full
conversation transcript via a subagent (keeping the raw transcript
out of your context).

## On Activation

### 1. Find the transcript path

The compaction summary includes a line like:

> read the full transcript at: /path/to/session.jsonl

Extract that path. If not found, check for `.jsonl` files in
`~/.claude/projects/` matching the current project directory.

### 2. Dispatch a research subagent

Launch a single `Agent` (general-purpose, foreground) with this
prompt structure:

```
Read the conversation transcript at [PATH] and extract:

1. **Last active task** — what was being worked on immediately
   before compaction? What was the literal next step?
2. **Nuanced decisions** — reasoning, tradeoffs, or constraints
   that informed choices (not just "chose A", but "chose A
   because B had X drawback")
3. **Warnings and corrections** — things the user corrected,
   scolded, or flagged as anti-patterns. These are easy to lose
   and critical to retain.
4. **Implicit commitments** — "we should also...", "don't
   forget...", "after this we need to..."
5. **Emotional/tonal context** — is the user in exploratory mode?
   Heads-down execution? Frustrated? Trusting autonomy?
6. **Key artifacts** — exact file paths, commit hashes, branch
   names, PR numbers, task IDs that are actively relevant

Focus on the MOST RECENT messages for the freshest context. The
file may be large — start from the end.

This is RESEARCH ONLY — do not edit or write any files.
```

### 3. Synthesize

When the subagent returns, present a concise synthesis to the
user. Structure:

```
## Context Recovered

**Active task**: [what + immediate next step]

**Key nuance recovered**:
- [2-5 bullets of decisions, warnings, context the summary missed]

**Implicit commitments**:
- [anything promised but not yet done]

Ready to continue.
```

## Do

- Use a foreground subagent (you need the results before proceeding)
- Focus the subagent on the END of the transcript (most recent = most relevant)
- Keep synthesis concise — bullets, not paragraphs
- Surface corrections and warnings prominently (they prevent repeat mistakes)

## Don't

- Read the transcript directly (the transcript is too large for your context)
- Duplicate what the compaction summary covers
- Present raw subagent output — synthesize it
- Offer next-step options (that's `/next` or `/debrief` territory)
- Ask the user questions — they invoked this to get context, not give it

---

Activate now. Find the transcript, dispatch the subagent, synthesize.
