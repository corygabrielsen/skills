---
name: loop-codex-review
description: Automated code review loop with progressive reasoning levels. Runs 3 parallel Codex reviews, Claude fixes issues, climbs from low→xhigh reasoning until fixed point (3 consecutive clean). Human approval at each iteration.
---

# Loop: Codex Review

You are a code review coordinator. **Codex reviews, Claude fixes.** Diverse LLM perspectives.

## Core Concept

```
┌─────────────────┐     ┌─────────────────┐
│  codex review   │────▶│  Claude fixes   │
│   (OpenAI CLI)  │     │  (Task agents)  │
└─────────────────┘     └─────────────────┘
         │                       │
         └───────── loop ────────┘
```

- **Review**: Run `codex review` command via Bash — this is OpenAI's Codex doing analysis
- **Fix**: Spawn Claude Task agents to resolve issues Codex found
- **Value**: Two different frontier LLMs catch different things

## Relationship to loop-address-pr-feedback

| Aspect | loop-codex-review | loop-address-pr-feedback |
|--------|-------------------|--------------------------|
| **When** | Pre-PR (local) | Post-PR (remote) |
| **Reviewer** | `codex review` CLI | GitHub bots + humans |
| **Trigger** | You run it | Reviews arrive async |
| **Interface** | stdout parsing | GitHub API |
| **Scope** | Single diff | Stack of PRs |
| **Fixed point** | n consecutive clean | All threads resolved |

Use **this skill** to validate code before opening a PR. Use **loop-address-pr-feedback** to address reviewer comments after.

## Reasoning Levels

Codex supports different reasoning effort levels. **Always set explicitly.**

```
┌─────────┬────────────────────────────────────────────┬──────────┐
│  Level  │  Description                               │  Time    │
├─────────┼────────────────────────────────────────────┼──────────┤
│  low    │  Quick scan - fast iteration, obvious bugs │  ~3 min  │
│  medium │  Moderate depth - good balance             │  ~5 min  │
│  high   │  Deep analysis - catches subtle issues     │  ~8-10m  │
│  xhigh  │  Exhaustive - maximum thoroughness         │  ~12-20m │
└─────────┴────────────────────────────────────────────┴──────────┘
```

**Command syntax:**
```bash
codex review --base master -c model_reasoning_effort="high"
```

⚠️ **Lower Reasoning Caveat**: Reviews at low/medium are faster but may miss subtle bugs. Real example: low and medium both returned clean (4 consecutive!), but high found a case-sensitivity bug (uppercase hex not normalized). **Always climb to at least high for production code.**

## Progressive Strategy (Default)

Default behavior: **Climb the reasoning ladder from low → xhigh**

```
low (n clean) → medium (n clean) → high (n clean) → xhigh (n clean) → DONE
     ↑                ↑                  ↑                 ↑
     └── bug found? fix and restart at this level ────────┘
```

Where `n` is the `-n` parameter (default: 3). Higher `n` = stronger fixed point = more confidence.

Why progressive?
- Fast feedback at low levels catches obvious issues quickly
- Each level validates the previous (higher levels catch what lower missed)
- User can stop early ("good enough, let's PR") but continuing is automatic
- Restarting a stopped loop is annoying; stopping a running one is easy

## Workflow Overview

```
1. Initialize       → Accept target (--base branch or --uncommitted)
2. Run codex review → Bash command, run_in_background: true
3. Parse Output     → Extract issues into tracker
4. Evaluate         → Issues found? Reset counter. Clean? Increment.
5. Check Exit       → n consecutive clean? → Done (graduate or finish)
6. Spawn Fix Agents → Claude agents fix Codex's findings (parallel)
7. Verify Fixes     → Tests pass, files modified
8. Human Approval   → Present diff, get explicit approval, commit
9. Loop             → Return to step 2
```

## State Schema

Track across iterations. Store in task descriptions for compaction survival.

```yaml
iteration_count: 0
review_mode: ""                    # --base master | --uncommitted | --commit SHA
review_criteria: ""                # Custom prompt passed to codex review
max_iterations: 15

# Reasoning level tracking
reasoning_level: "low"             # Current: low | medium | high | xhigh
reasoning_strategy: "progressive"  # progressive | fixed
consecutive_clean_at_level: 0      # Resets on any finding
consecutive_clean_target: 3        # -n flag (default 3, max 10)
reviews_per_batch: 3               # Run 3 in parallel

# Level history (for reporting)
level_history:
  low:    { reviews: 0, bugs_found: 0, fixed_point: false }
  medium: { reviews: 0, bugs_found: 0, fixed_point: false }
  high:   { reviews: 0, bugs_found: 0, fixed_point: false }
  xhigh:  { reviews: 0, bugs_found: 0, fixed_point: false }

issue_tracker: []
```

## Phase: Initialize

**On activation:**

1. Determine review mode from args:
   - No args or directory → `--uncommitted` (review working changes)
   - `--base <branch>` → review changes vs branch
   - `--pr <num>` → `--base` against PR's target branch
   - `--commit <sha>` → review specific commit

2. **Detect base branch** (don't naively assume master):
   ```bash
   # Check if in a Graphite stack
   gt ls 2>/dev/null
   ```
   - If in a stack, the base is the **parent branch**, not master
   - Use `gt log --oneline` or check PR target to find actual base
   - Only the bottom of a stack targets master/main

3. Parse optional criteria (custom review prompt)

4. Initialize state, create tracking task

**Base branch detection:**
```
Stack example (gt ls):
  ◉ feature-c  ← current (base: feature-b)
  ◉ feature-b  (base: feature-a)
  ◉ feature-a  (base: master)
  ◉ master

In this case, reviewing feature-c should use --base feature-b, NOT --base master.
```

**Args examples:**
```bash
# Default: progressive low → xhigh, 3 consecutive clean per level
/codex-review-loop                          # --uncommitted, full climb
/codex-review-loop --base master            # Review vs master, full climb

# Start at specific level
/codex-review-loop --level high             # Start at high, climb to xhigh
/codex-review-loop --level xhigh            # Start and stay at xhigh only

# Fixed level (no climbing)
/codex-review-loop --level medium --no-climb  # Stay at medium only

# Quick mode (low only, for fast iteration during development)
/codex-review-loop --quick                  # Alias for --level low --no-climb

# Fixed point strength: -n sets consecutive clean reviews needed per level
/codex-review-loop -n 10                    # High confidence (10 clean per level)
/codex-review-loop -n 1                     # Fast/yolo mode (1 clean per level)
/codex-review-loop --quick -n 1             # Fastest possible (low only, 1 clean)

# With custom criteria
/codex-review-loop "check for security issues" --level high

# Auto-detect base from Graphite stack
/codex-review-loop --base auto              # Uses gt to find parent branch
```

**The `-n` parameter:** Controls how many consecutive clean reviews are needed before declaring fixed point at each level. Default is 3. Higher values give more confidence but cost more. Max recommended is 10.

**Auto-detection logic:**
1. If `gt` available → check parent with `gt log --oneline -n 1` or parse `gt ls`
2. Else if in PR → use `gh pr view --json baseRefName`
3. Else → fall back to master/main

## Phase: Review (THE KEY PART)

**This runs the actual `codex review` CLI command — NOT a Claude agent.**

### Always Run 3 Reviews in Parallel

Spawn 3 simultaneous reviews at the current reasoning level:

```
Task(subagent_type: "Bash", run_in_background: true, prompt: "codex review --base master -c model_reasoning_effort=\"low\"")
Task(subagent_type: "Bash", run_in_background: true, prompt: "codex review --base master -c model_reasoning_effort=\"low\"")
Task(subagent_type: "Bash", run_in_background: true, prompt: "codex review --base master -c model_reasoning_effort=\"low\"")
```

**Fixed point = n consecutive clean (default 3).** If ANY review finds bugs, fix and restart the count.

### Command Construction

| Mode | Command |
|------|---------|
| uncommitted | `codex review --uncommitted -c model_reasoning_effort="high"` |
| vs branch | `codex review --base <branch> -c model_reasoning_effort="high"` |
| vs stack parent | `codex review --base feature-b -c model_reasoning_effort="high"` |
| specific commit | `codex review --commit abc123 -c model_reasoning_effort="high"` |
| with criteria | `codex review --uncommitted "check for SQL injection" -c model_reasoning_effort="high"` |

**Important:** When in a Graphite stack, always review against the parent branch, not master.

### Example: Launch 3 Parallel Reviews

```python
# In a single message, spawn all 3:
Task(
  description: "Codex review 1/3 (high)",
  prompt: "Run: codex review --base master -c model_reasoning_effort=\"high\" 2>&1",
  subagent_type: "Bash",
  run_in_background: true
)
Task(
  description: "Codex review 2/3 (high)",
  prompt: "Run: codex review --base master -c model_reasoning_effort=\"high\" 2>&1",
  subagent_type: "Bash",
  run_in_background: true
)
Task(
  description: "Codex review 3/3 (high)",
  prompt: "Run: codex review --base master -c model_reasoning_effort=\"high\" 2>&1",
  subagent_type: "Bash",
  run_in_background: true
)
```

**Record the task IDs** from each launch (e.g., `a1b2c3d`). Notifications are unreliable — you'll need to poll the output files directly. See "Handling Zombie Tasks" section.

## Phase: Parse Output

Codex review outputs markdown with issues. Parse into tracker:

```markdown
| ID | File | Line | Severity | Description | Status | Round | Level |
|:--:|:-----|:----:|:--------:|:------------|:------:|:-----:|:-----:|
| CR-001 | src/auth.js | 42 | major | SQL injection | open | R1 | high |
```

### Evaluate 3 Parallel Results

```
results = [review_1, review_2, review_3]

if ALL results are clean:
    consecutive_clean_at_level += 3
    if consecutive_clean_at_level >= 3:
        # Fixed point at this level!
        if reasoning_level == "xhigh":
            → DONE (full fixed point reached)
        else:
            → Escalate to next level, reset counter
else:
    # ANY review found bugs
    consecutive_clean_at_level = 0
    → Merge all bugs into tracker, proceed to fix phase
```

### Verification of Findings

⚠️ **Verify findings carefully, especially from lower reasoning levels.**

Before fixing, sanity-check each issue:
- Is this a real bug or false positive?
- Does the suggested fix make sense?
- Could this be an intentional design decision?

**Triage each finding:**

| Finding Type | The Fix |
|--------------|---------|
| Real bug | Fix the code |
| False positive | Fix the documentation (add comments explaining the intent) |
| Design tradeoff | Document the rationale in code comments |
| Unclear | Research before deciding |

**Critical insight: False positives are documentation bugs.**

When a reviewer misunderstands your code, the code is unclear. If an LLM gets confused, a tired human will too. The fix is NOT to dismiss the finding — it's to add comments or refactor until the intent is obvious.

Example: A reviewer flags an empty `catch` block as "swallowing errors." But you're intentionally ignoring that specific error. The fix isn't to dismiss the review — it's to add a comment:
```javascript
} catch (e) {
  // Intentionally ignored: retries handle this upstream
}
```

Now the next reviewer (human or LLM) won't raise the same concern. The false positive becomes impossible.

### Synthesize Before Fixing

⚠️ **Always zoom out before fixing.**

Reviewers do deep analysis but output terse summaries. A finding that looks like a one-line fix often touches code with multiple exit paths, callers, and implicit contracts. Fixing the symptom without understanding the system leads to incomplete or wrong fixes.

This step is not optional, and it's not just for "complex" findings. Even when a single reviewer flags a single line, ask: why was this subtle enough that others missed it? What else in this area might have similar issues?

**The protocol:**

1. **Read the full context** — Not just the flagged line. Read the entire function, its callers, and sibling code. The summary is a pointer; the truth is in the source.

2. **Map the system** — Trace the relevant paths:
   - All exit points from the function
   - All callers and call sites
   - All reads and writes of affected state

3. **Look for patterns** — Findings in the same file or touching the same concept (error handling, validation, cleanup) may share a root cause. A single finding may reveal a pattern repeated elsewhere.

4. **Ask the hard questions:**
   - What contract should this code uphold?
   - Does every path honor that contract?
   - What would a surface-level fix miss?
   - Is there a structural issue underneath?

5. **Challenge yourself** — "Is this my best effort? What haven't I considered?"

The goal is to reconstruct the full picture before acting. Understand the system, then fix holistically.

## Phase: Fix (Claude Agents)

**Exit check first:**
```
if consecutive_clean_at_level >= 3 AND reasoning_level == "xhigh":
    → Done (full fixed point reached)
if consecutive_clean_at_level >= 3:
    → Escalate to next reasoning level
if iteration_count >= max_iterations:
    → Ask user how to proceed
```

### When Bugs Found: Ask User for Strategy

Use `AskUserQuestion` to let user choose restart strategy:

```
"Found [N] issues at [level] reasoning. After fixing, how should we verify?"

Options:
1. "Re-review at [current level]" (recommended) - Verify fix at same depth
2. "Restart from low" - Full re-climb, maximum confidence
3. "Drop one level and re-climb" - Balance of speed and thoroughness
4. "Skip to next level" - Trust the fix, continue climbing
```

Context matters: A subtle edge case found at `high` probably just needs re-review at `high`. A fundamental bug that `low` should have caught might warrant a full restart.

### Spawn Claude Fix Agents

**Spawn fix agents in parallel** via Task tool:
- One agent per issue (or grouped by file)
- `run_in_background: true` for parallel execution
- Agent prompt includes issue details from Codex's review

```
Task(
  description: "Fix CR-001: SQL injection",
  prompt: "Fix the SQL injection issue found by code review...",
  subagent_type: "general-purpose",
  run_in_background: true
)
```

## Phase: Verify

After fix agents complete:
1. Run tests (`make test` or equivalent)
2. Verify files were modified
3. Update issue tracker: `open` → `fixed`

## Phase: Human Approval

**Present detailed summary with enough context to make an informed decision:**

```markdown
## Iteration {N} — Detailed Review

### CR-001: [Short title] (severity)

**The Bug:**
[2-3 sentences explaining what the bug is, where it occurs, and why it matters.
Include a code snippet if it helps illustrate the problem.]

**The Fix:**
[1-2 sentences or code snippet showing what changed and why this resolves it.]

**Impact:** [One line on what this fixes for users]

---

### CR-002: [Short title] (severity)

**The Bug:**
[Same format...]

**The Fix:**
[Same format...]

**Impact:** [...]

---

### Summary

| ID | File | Change |
|----|------|--------|
| CR-001 | src/auth.js | String concat → parameterized query |
| CR-002 | src/api.ts | Added null check before access |

### Claude's Fixes
- **CR-001**: Replaced string concatenation with parameterized query
- **CR-002**: Added null coalescing with sensible default

### Verification
- [x] Tests passing (N/N)
- [x] Files modified: src/auth.js, src/api.ts
```

**Key principle:** The human needs enough context to understand *what* the bug was, *why* it matters, and *how* Claude fixed it — without having to dig through logs or diffs.

**AskUserQuestion with options:**
1. "Approve and commit" — commit fixes, continue to next review
2. "View full diff" — show `git diff`, then re-ask
3. "Request changes" — user specifies modifications
4. "Abort" — exit loop, keep changes uncommitted

## Phase: Commit

After human approval:

```bash
git add -A && git commit -m "$(cat <<'EOF'
codex-review: Fix issues from iteration N

Issues resolved:
- CR-001: SQL injection in auth.js (major)

Reviewed by: OpenAI Codex
Fixed by: Claude

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>
EOF
)"
```

Then loop back to Phase: Review.

## Fixed Point

### The True Definition

A **true fixed point** requires BOTH:
1. **No real bugs** — the code is correct
2. **No false positives** — the code is clear enough that reviewers understand it

**False positives are bugs in your documentation, not bugs in the reviewer.**

If 1 in 10 reviewers misunderstands your code, that's a 10% confusion rate. Fix it by adding comments until the confusion rate hits 0%. Don't dismiss — clarify, then re-run to verify.

### Per-Level Fixed Point
When `consecutive_clean_at_level >= 3` at any level:
```
3 parallel reviews at [level] found nothing.
Fixed point at [level]. Escalating to [next level]...
```

### Full Fixed Point
When `consecutive_clean_at_level >= 3` at `xhigh`:
```
┌─────────────────────────────────────────────────────────┐
│  FULL FIXED POINT REACHED                               │
├─────────────────────────────────────────────────────────┤
│  low:    3/3 clean ✓                                    │
│  medium: 3/3 clean ✓                                    │
│  high:   3/3 clean ✓                                    │
│  xhigh:  3/3 clean ✓                                    │
├─────────────────────────────────────────────────────────┤
│  Total reviews: 12  |  Bugs found & fixed: N            │
│  Code has been validated at all reasoning depths.       │
└─────────────────────────────────────────────────────────┘
```

Report final summary with level history and exit.

## Issue Tracker Format

Maintain throughout session:

```
┌────────┬─────────────┬──────┬──────────┬─────────────────────┬────────┬───────┐
│ ID     │ File        │ Line │ Severity │ Description         │ Status │ Round │
├────────┼─────────────┼──────┼──────────┼─────────────────────┼────────┼───────┤
│ CR-001 │ src/auth.js │ 42   │ major    │ SQL injection       │ fixed  │ R1    │
│ CR-002 │ src/api.ts  │ 108  │ minor    │ Missing null check  │ fixed  │ R1    │
│ CR-003 │ src/util.js │ 15   │ style    │ Unused import       │ wontfix│ R2    │
└────────┴─────────────┴──────┴──────────┴─────────────────────┴────────┴───────┘
```

Severities: `critical` | `major` | `minor` | `style`
Statuses: `open` | `fixing` | `fixed` | `wontfix`

## Resumption (Post-Compaction)

1. Run `TaskList` to find review loop task
2. Read task description for persisted state
3. Check for running background Bash (codex review) or Task agents
4. Resume from appropriate phase

## Handling Zombie Tasks

⚠️ **Background task notifications are unreliable.** ~50% of notifications are lost when you're mid-response. Don't trust them.

**The Pattern:**

1. **Record IDs when launching** — Note the task IDs from each Task() call
2. **Wait briefly** — Give tasks time to run (~3-5 min for medium, ~15 min for xhigh)
3. **Poll proactively** — Don't wait for notifications; check the files directly
4. **If user says "tasks are done"** — Trust them and poll immediately

**Polling Recipe:**
```bash
# Check if task is complete (type=assistant means done, type=progress means running)
tail -1 /tmp/claude/.../tasks/<id>.output | python3 -c "import sys,json; print(json.load(sys.stdin).get('type','unknown'))"

# Extract result from completed task
tail -1 /tmp/claude/.../tasks/<id>.output | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['message']['content'][0]['text'][-1500:])"

# Batch check all tasks
for id in <id1> <id2> <id3>; do echo "=== $id ==="; <extract command>; done
```

**Key insight:** Treat notifications as hints, not guarantees. When in doubt, poll.

## Contradictory Findings

When successive reviews recommend opposing changes, this signals genuine design tension:

1. **Pause** — Don't implement the latest suggestion reflexively
2. **Enumerate solutions** — Map all approaches with their tradeoffs
3. **Clarify requirements** — Use AskUserQuestion to understand which constraints are hard vs soft
4. **Search for synthesis** — Often a solution exists that satisfies multiple constraints
5. **Commit deliberately** — If no synthesis exists, choose and document the rationale

Contradictory findings usually indicate underspecified requirements, not wrong reviews.

## Anti-patterns

- Running Claude agents to "review" — USE `codex review` CLI
- Skipping human approval checkpoint
- Committing without reaching fixed point (3 clean at current level)
- Running codex review without `run_in_background` (takes 3-20 min)
- Fixing issues user marked `wontfix`
- Infinite loops without max_iterations guard
- **Forgetting to set `-c model_reasoning_effort`** — always explicit
- **Running reviews sequentially** — always 3 in parallel
- **Trusting low/medium clean reviews as "done"** — always climb to at least high
- **Not verifying findings before fixing** — especially at lower reasoning levels
- **Stopping at first fixed point** — default is full climb to xhigh
- **Dismissing false positives without improving code** — add comments to clarify confusing logic
- **Blaming the reviewer for misunderstanding** — if an LLM gets confused, a human might too

---

Enter codex-review-loop mode now. Parse args for review mode and starting level (default: low, climbing to xhigh). Launch 3 parallel `codex review` commands via Task tool with `run_in_background: true`. Always set `-c model_reasoning_effort` explicitly. Do NOT do the review yourself — delegate to Codex via the CLI.
