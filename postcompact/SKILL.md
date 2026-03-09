---
name: postcompact
description: Recover nuance lost during conversation compaction by reading the full transcript via a subagent.
---

# Postcompact

Compaction summaries preserve facts but lose nuance. This
skill recovers what was lost by dispatching a subagent to
read the full conversation transcript.

## On Activation

### 1. Find the transcript

The compaction summary includes a line like:

> read the full transcript at: /path/to/session.jsonl

Extract that path. If not found, check for `.jsonl` files
in `~/.claude/projects/` matching the current project.

### 2. Dispatch a research subagent

Launch a foreground `Agent` to read the transcript and
extract what the compaction summary missed:

- What was being worked on and what was the next step
- Reasoning behind decisions, not just the decisions
- Corrections and warnings from the user
- Implicit commitments ("we should also...", "after this...")
- Tone — exploratory? heads-down? frustrated?
- Active artifacts — file paths, branches, commits, PRs

Tell the agent to focus on the most recent messages and
to do research only (no edits).

### 3. Synthesize and present

Distill what the subagent found into what matters for
continuing the conversation. Don't parrot the raw output.

## Do

- Use a foreground subagent (you need results before proceeding)
- Prioritize the end of the transcript (most recent = most relevant)
- Surface corrections and warnings prominently

## Don't

- Read the transcript directly (too large for your context)
- Duplicate what the compaction summary already covers
- Ask questions — the user wants context recovered, not a quiz
