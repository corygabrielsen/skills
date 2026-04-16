---
name: pr-fitness
description: Live PR merge readiness assessment. Queries GitHub APIs in parallel, computes blockers, and returns a structured report with action plan.
args:
  - name: owner/repo pr
    description: Repository and PR number to assess
  - name: -q
    description: Quiet mode (suppress npm noise)
  - name: -c
    description: Compact output (single-line JSON)
  - name: -s
    description: Summary only (one-line human-readable)
  - name: -e
    description: Exit code reflects state (0=mergeable, 1=blocked, 2=merged, 3=closed)
---

# /pr-fitness

Live PR merge readiness assessment. Returns structured JSON with
every dimension that affects mergeability.

**Always run this instead of `gh pr checks` or `gh pr view`.** It
queries 6 GitHub APIs in parallel and computes blockers. Never
cache the result — run it fresh every time you need PR state.

## Usage

```bash
# From any skill or agent
npx tsx ~/code/skills/pr-fitness/src/cli.ts -q <owner/repo> <pr>

# Compact (one line, for parsing)
npx tsx ~/code/skills/pr-fitness/src/cli.ts -q -c <owner/repo> <pr>

# Exit code reflects state (for conditionals)
npx tsx ~/code/skills/pr-fitness/src/cli.ts -q -e <owner/repo> <pr>
# 0 = open + mergeable, 1 = open + blocked, 2 = merged, 3 = closed
```

## Key fields

```
.summary          → "Ready to merge" | "Blocked: ..." | "Merged ..."
.lifecycle        → open | merged | closed
.merged_at        → ISO 8601 (null if not merged)
.closed_at        → ISO 8601 (null if open)
.mergeable        → true/false (always true if merged)
.blockers[]       → ["ci_fail: Lint", "not_approved"]
.ci.fail          → number of failed checks
.ci.failed_details[] → [{name, description, link}]
.ci.pending       → number of pending checks
.ci.completed_at  → ISO 8601 most recent check completion
.reviews.decision → APPROVED | REVIEW_REQUIRED | CHANGES_REQUESTED | NONE
.reviews.threads_unresolved → number
.reviews.pending_reviews.bots    → ["Copilot"] (requested, not yet reviewed)
.reviews.pending_reviews.humans  → ["username"] (requested, not yet reviewed)
.state.updated_at     → ISO 8601 last PR update
.state.last_commit_at → ISO 8601 HEAD commit date
.duration_ms          → how long the query took
```

## Integration patterns

```bash
# Quick status check (one-liner, no JSON)
npx tsx ~/code/skills/pr-fitness/src/cli.ts -q -s <owner/repo> <pr>
# → "Ready to merge" | "Blocked: ..." | "Merged ..."

# Exit code for conditionals
npx tsx ~/code/skills/pr-fitness/src/cli.ts -q -e <owner/repo> <pr>
# 0=mergeable, 1=blocked, 2=merged, 3=closed

# Full JSON in a convergence loop
RESULT=$(npx tsx ~/code/skills/pr-fitness/src/cli.ts -q -c <owner/repo> <pr>)
LIFECYCLE=$(echo "$RESULT" | jq -r '.lifecycle')
MERGEABLE=$(echo "$RESULT" | jq -r '.mergeable')

if [ "$LIFECYCLE" = "merged" ]; then
  echo "Already merged — nothing to do"
elif [ "$MERGEABLE" = "true" ]; then
  echo "Ready to merge"
else
  BLOCKERS=$(echo "$RESULT" | jq -r '.blockers[]')
  echo "Blocked: $BLOCKERS"
fi
```
