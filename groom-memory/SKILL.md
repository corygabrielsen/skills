---
name: groom-memory
description:
  Audit and prune the per-project auto-memory directory. Goal is small,
  in-context principles + pointers to authoritative sources (issue tracker, repo,
  dashboards) — not duplication. Socratic when ambiguous; don't autopilot.
---

# Groom memory

The per-project auto-memory directory drifts over time. New memos pile up; closed
work lingers; the same lesson gets restated in new vocabulary; content that
should live elsewhere (issue tracker, repo docs, dashboards) ends up duplicated.
Grooming reverses the drift.

## When to invoke

- Memory feels bloated.
- After a long session that produced many new memos.
- When linking a session to an external authority — tracker project, repo,
  dashboard — for the first time.
- When the project focus has shifted and memory holds historical-but-not-current
  entries.

## What memory IS for

- **In-context principles** that should shape future sessions (writing style,
  design discipline, failure-mode recognition).
- **Pointers to authoritative sources** — tracker project IDs + query recipes,
  repo doc paths, dashboard URLs.
- **Reference cards** for non-obvious traps you'll hit again.

## What memory is NOT for

- Duplicating tracker state (tickets, milestones, descriptions).
- Long-form design docs — those belong in the repo or in ticket bodies.
- Session narratives ("I tried X, then Y") — those belong in PR descriptions.
- Restating the same lesson with new vocabulary each session.

## How to start

Audit, then decide. Don't autopilot.

1. Read `MEMORY.md` if present. Inventory files by category (feedback, project,
   reference, operational, user).
2. For each project-state file, ask: where does the canonical state actually
   live? Tracker? Repo? Dashboard?
3. Skim feedback files for clusters — many directories have 5-8 entries that are
   different framings of one lesson.
4. Identify stale work (closed migrations, shelved decisions, completed
   projects).

## Ask when ambiguous (Socratic)

Memory pruning has irreversible deletes. The most common ambiguities:

- "Is this content already in the tracker / the repo?"
- "Is this project still active, or are we shifting focus going forward?"
- "Should this design doc move to the repo, or be deleted entirely?"
- "What's the actual project name now? The tracker's name may have drifted from
  the current focus."
- "Which clusters of repeated lessons should collapse, and which should stay
  separate?"

Use `AskUserQuestion` with 2-4 options + tradeoffs. Don't over-design — surface
intent, don't templatize.

## Common moves after grooming

The user may want to:

- Rename a tracker project to match the new focus.
- Update ticket / milestone descriptions to drop dead auto-memory references
  (auto-memory fails the stranger test for shared surfaces).
- Cancel tickets that describe path-not-taken work.
- Create a reference index entry that maps the tracker project so future sessions
  know where to query for live state.

Surface these as questions. Don't autopilot.

## Tracker linkage (when the project has one)

When an issue tracker holds the project:

1. Identify the project (slug, ID, milestones, key issues).
2. Create a single `reference_<tracker>_<project>_index.md` memory entry holding
   the project ID + milestone IDs + active issue summaries + query recipes.
3. Delete memory files that duplicate the tracker's ticket/milestone state.
4. Update the tracker's project description if memory pruning revealed it had
   drifted from the actual focus.

## Anti-patterns

- **Rigid procedural template.** Memory shapes vary by project. Don't impose
  fixed structure.
- **Cargo-cult relocation.** If memory holds design docs, ask whether the repo
  SHOULD have them — don't just move scratchpads into the repo.
- **Add memos to fix bloat.** Memo count grows when writes outpace consolidation.
  Cut > add.
- **Memo creation as progress.** The unit of progress is "future-me picks up
  faster". Fewer sharper memos beats many wandering ones.
