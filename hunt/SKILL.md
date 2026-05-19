---
name: hunt
description: Fan-out subagent search-and-fix for a class of issue. Find, categorize, saturate, stack, linearize.
args:
  - name: <target>
    description: What to hunt for (e.g. "perf issues", "deprecated APIs", "missing tests", "type smells")
---

# /hunt

We're going to hunt for `<target>`. Do this:

1. **INITIAL HUNT**: Using fan-out subagents, find 10 instances in 10 minutes.
2. **CATEGORIZE**: Classify and group the findings into categories.
3. **SATURATE**: Study each category, generalize its principles, and saturate the fix methodology across the full codebase.
4. **STACKED DIFFS**: Rank and stack fixes from simplest to most complex. Prepare branches.
5. **LINEARIZE**: Merge and resolve conflicts.
