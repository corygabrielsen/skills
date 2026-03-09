---
name: precompact
description: Persist current session knowledge to durable storage. Update memory files, clean stale state, ensure what you know is written down.
---

# Precompact

Persist what you know to durable storage. Memory files
should reflect reality — not a stale snapshot from three
tasks ago.

## On Activation

Work through these steps. Skip any that don't apply.

### 1. Update memory files

Review your auto-memory directory and update it:

- **Active work**: branches, commit hashes, what's done vs
  pending, current branch relationships
- **Decisions made**: anything decided in this session that
  isn't written down yet
- **Corrections received**: if the user corrected you,
  memory must reflect the correction
- **Remove stale entries**: merged PRs listed as open, old
  branch names, completed tasks, outdated notes — fix them

### 2. Clean up task list

If tasks were used in this session:

- Mark completed tasks as completed
- Remove tasks that are no longer relevant
- Update in-progress descriptions if scope changed

### 3. Persist undocumented decisions

Scan the conversation for things decided but not yet
written anywhere durable:

- Naming conventions agreed on
- Architectural choices made
- Patterns established
- User preferences expressed

Write these to the appropriate memory file.

### 4. Report what you did

## Arguments

If the user passes arguments, treat them as explicit
instructions to persist before doing the rest.

## Do

- Be thorough — if you know it and it matters, write it down
- Remove stale information (wrong is worse than missing)
- Update hashes, branch names, PR statuses to current values

## Don't

- Create new files unless memory genuinely needs a new topic
- Ask questions — persist what you know, not what you don't
- Touch code or make changes beyond memory/task housekeeping
