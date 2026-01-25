---
name: loop-review-skill-parallel
description: Iterate skill document review until fixed point. Runs n parallel reviewers per iteration (via review-skill-parallel), addresses findings, repeats until all n reviewers return clean. Human approval at each iteration.
---

# Loop: Review Skill (Parallel)

You are a skill document review coordinator. **Task agents review, you address.** Multiple identical reviewers catch different issues through execution diversity.

## Core Philosophy

**Every finding demands document improvement. No exceptions.**

When a reviewer flags something, the document changes. Always. Either:
- **Real inconsistency** → fix the document
- **False positive** → the document was unclear; rewrite until the intent is obvious
- **Design tradeoff** → document the rationale explicitly

There is no "dismiss," no "accept risk," no "wontfix." If a reviewer misunderstood, that's a signal the document isn't self-evident — another LLM executing this skill would misunderstand too. The document must become clearer.

**Fixed point** = no reviewer can find *anything* to flag. Not because you argued them down, but because the document is both **correct** AND **self-evident**.

---

## Core Concept

```
┌─────────────────┐     ┌───────────────────┐
│  n Task agents  │────▶│  Claude addresses │
│  (fungible)     │     │   (editor)        │
└─────────────────┘     └───────────────────┘
         │                       │
         └───────── loop ────────┘
```

- **Review**: Launch n parallel Task agents with identical prompts
- **Address**: Make edits to the skill document (fix inconsistencies OR clarify wording)
- **Loop**: Repeat until all n reviewers return clean (fixed point)
- **Value**: Multiple identical reviewers catch different issues through execution diversity

## Relationship to review-skill-parallel

| Aspect | review-skill-parallel | loop-review-skill-parallel |
|--------|----------------------|----------------------------|
| **Scope** | Single iteration | Iterate until fixed point |
| **Output** | Findings addressed, human approved | Document at fixed point |
| **When** | Called by this wrapper | User invokes directly |

This skill **wraps** review-skill-parallel, running it repeatedly until fixed point.

## State Schema

Track across iterations:

```yaml
target_file: ""              # Path to skill file being edited
parallel_review_count: 3     # -n flag (default 3)
max_iterations: 10           # Safety limit
iteration_count: 0           # Current iteration
total_findings_addressed: 0  # Cumulative count
iteration_history: []        # [{iteration: 1, findings: 3}, ...]
```

---

## Phase: Initialize

### Do:
- Accept target skill file path from args
- Validate file exists and is a SKILL.md
- Initialize state and create tracking task

### Don't:
- ❌ Start without a target file — require explicit path
- ❌ Review non-skill files — this skill is for SKILL.md files only

**On activation:**

1. Parse args for target file:
   ```
   /loop-review-skill-parallel path/to/SKILL.md           # Review with 3 parallel reviewers
   /loop-review-skill-parallel path/to/SKILL.md -n 5      # 5 parallel reviewers
   ```

2. Validate target exists and contains YAML frontmatter with `name:` field

3. Initialize state, create tracking task

**Args:**
- First positional arg: path to SKILL.md (required)
- `-n <count>`: number of parallel reviewers (default: 3)

---

## Phase: Iteration Loop

**Run iterations until fixed point or max iterations reached.**

### Do:
- Check exit conditions before each iteration
- Run full review-skill-parallel cycle each iteration
- Track findings per iteration
- Persist state for compaction survival

### Don't:
- ❌ Skip exit check — you might already be done
- ❌ Exceed max iterations without asking user

### Iteration Protocol

```
for each iteration:
    1. Check exit conditions
    2. Launch n parallel review agents (fungible, same prompt)
    3. Wait for all n to complete
    4. Parse and synthesize findings
    5. If ALL n clean → fixed point reached, exit loop
    6. Else → address findings, verify, get human approval
    7. Increment iteration, loop
```

### Exit Conditions

```
if all_n_clean:
    → Fixed point reached — exit loop with success
if iteration_count >= max_iterations:
    → Ask user how to proceed (continue, stop, adjust)
```

---

## Phase: Review (Per Iteration)

**Launch n parallel Task agents to review the skill document. All agents are fungible — same prompt.**

### Do:
- Use `Task` tool with `run_in_background: true`
- Launch all n agents in a **single message** (parallel)
- Use identical prompt for all agents
- Record all agent IDs for result collection

### Don't:
- ❌ Run reviews sequentially — always parallel
- ❌ Do the review yourself — delegate to Task agents
- ❌ Customize prompts per agent — all reviewers are fungible

### Review Prompt Template

All agents receive the same prompt:

```
You are reviewing {file} for internal consistency and clarity issues.

This is a skill document that instructs an LLM how to perform a task.
We want to reach a "fixed point" where no reviewer can find anything to flag.

Look for:
- Terminology inconsistencies (e.g., same concept with different names)
- Contradictions between sections
- Unclear or ambiguous instructions
- Structural issues (missing sections, formatting inconsistencies)
- Philosophy not consistently applied

Read the full file carefully. Report ANY findings, no matter how small.

Output format:
FINDINGS:
1. Line X: [issue description]
2. Line Y: [issue description]
...

OR

NO FINDINGS - document is internally consistent.
```

### Example: Launch n=3 Parallel Review Agents

```
Task(
  description: "Review SKILL.md iteration {N} (1/3)",
  prompt: "[review prompt with {file} substituted]",
  subagent_type: "general-purpose",
  run_in_background: true
)
Task(
  description: "Review SKILL.md iteration {N} (2/3)",
  prompt: "[same review prompt]",
  subagent_type: "general-purpose",
  run_in_background: true
)
Task(
  description: "Review SKILL.md iteration {N} (3/3)",
  prompt: "[same review prompt]",
  subagent_type: "general-purpose",
  run_in_background: true
)
```

Each Task returns an agent ID. Wait for all agents to complete before proceeding.

**Fixed point = all n clean.** If ANY review has findings, address them and iterate again.

---

## Phase: Parse Output

**Collect results from all n agents and synthesize into a unified findings list.**

### Do:
- Wait for all n agents to complete
- Extract findings from each agent's output
- Merge into issue tracker, deduplicating similar findings
- Record which agents found each issue

### Don't:
- ❌ Skip findings because they seem minor — every finding gets tracked
- ❌ Proceed before all agents complete — wait for all n

### Evaluate n Parallel Results

```
results = [agent_1, agent_2, ..., agent_n]

if ALL n results are "NO FINDINGS":
    → Fixed point reached — exit loop
else:
    # ANY agent has findings
    → Merge all findings into tracker
    → Proceed to Address phase
```

### Issue Tracker Format

```markdown
| ID | Line | Description | Status | Agent | Iter |
|:--:|:----:|:------------|:------:|:-----:|:----:|
| F-001 | 42 | Terminology: "bug" vs "finding" inconsistent | open | 1,3 | I1 |
| F-002 | 108 | Missing Do/Don't section in Phase: Verify | open | 2 | I1 |
```

Statuses: `open` | `addressing` | `fixed` | `clarified`

### Verification of Findings

**Do:**
- Verify each finding before addressing
- Ask: real inconsistency, false positive, or design tradeoff?
- Triage using this table:

| Finding Type | The Resolution |
|--------------|----------------|
| Real inconsistency | Fix the document |
| False positive | Rewrite until the intent is obvious |
| Design tradeoff | Document the rationale explicitly |

**Don't:**
- ❌ Address without verifying first
- ❌ Dismiss findings without improving document — every finding = document change
- ❌ Blame the reviewer for misunderstanding — if an LLM got confused, another will too

---

## Phase: Address

**Make edits to the skill document to resolve all findings.**

### Do:
- Address all open findings from the tracker
- Use Edit tool for targeted changes
- Group related findings when they share a root cause
- Update tracker status as you go

### Don't:
- ❌ Skip any finding — every finding demands document improvement
- ❌ Make changes without reading the relevant sections first
- ❌ Over-edit — make minimal changes to resolve each finding

### Address Protocol

For each finding:

1. **Read context** — Read the section(s) containing the finding
2. **Identify resolution** — Fix, clarify, or document rationale
3. **Make edit** — Use Edit tool with precise old_string/new_string
4. **Update tracker** — Mark as `fixed` or `clarified`

---

## Phase: Verify

**Verify all changes were made correctly.**

### Do:
- Re-read all sections that were modified
- Confirm each finding was properly addressed
- Check for unintended side effects from edits
- Ensure tracker shows all findings as `fixed` or `clarified`

### Don't:
- ❌ Skip verification — always re-read modified sections
- ❌ Proceed with open findings — all must be addressed

### Verification Checklist

```
[ ] All findings in tracker are `fixed` or `clarified`
[ ] Re-read each modified section
[ ] No new inconsistencies introduced by edits
[ ] Document still parses correctly (YAML frontmatter valid)
```

---

## Phase: Human Approval

**Present summary to user and get explicit approval before continuing to next iteration.**

### Do:
- Present detailed summary with all findings and resolutions
- Use AskUserQuestion with clear options
- Wait for explicit approval

### Don't:
- ❌ Skip this checkpoint — human approval is mandatory
- ❌ Assume approval — wait for explicit response

### Summary Template

```markdown
## Iteration {N} Summary

### Findings: {count}

| ID | Line | Issue | Resolution |
|----|------|-------|------------|
| F-001 | 42 | "bug" vs "finding" inconsistency | Standardized on "finding" |
| F-002 | 108 | Missing Do/Don't section | Added Do/Don't to Phase: Verify |

### Changes Made
- Lines X-Y: [description]
- Lines A-B: [description]

### Verification
- [x] All findings addressed
- [x] Modified sections re-read
- [x] No new inconsistencies
```

### Approval Options

Use AskUserQuestion:
1. **"Approve and continue"** — Commit changes, run next iteration
2. **"View diff"** — Show full changes, then re-ask
3. **"Request changes"** — User specifies modifications
4. **"Stop here"** — Exit loop with current state

---

## Phase: Fixed Point

**When all n reviewers return clean, declare fixed point and exit.**

### Do:
- Verify ALL n reviews returned "NO FINDINGS"
- Present final summary with iteration history
- Celebrate the achievement

### Don't:
- ❌ Declare fixed point if ANY review has findings
- ❌ Skip final summary — user needs to see the journey

### Fixed Point Report

```markdown
┌─────────────────────────────────────────────────────────┐
│  FIXED POINT REACHED                                     │
├─────────────────────────────────────────────────────────┤
│  Iterations: {N}                                         │
│  Total findings addressed: {count}                       │
│  Final review: {n}/{n} reviewers clean                   │
├─────────────────────────────────────────────────────────┤
│  Iteration History:                                      │
│    I1: 3 findings → addressed                            │
│    I2: 1 finding → addressed                             │
│    I3: 0 findings → CLEAN                                │
├─────────────────────────────────────────────────────────┤
│  Document is internally consistent and self-evident.     │
└─────────────────────────────────────────────────────────┘
```

---

## Resumption (Post-Compaction)

1. Run `TaskList` to find review loop task
2. Read task description for persisted state
3. Check for running background Task agents
4. Resume from appropriate phase

---

## Quick Reference: Don'ts

Pre-flight checklist. Details are inline in each phase above.

| Phase | Don't |
|-------|-------|
| Initialize | Start without target file, review non-skill files |
| Iteration Loop | Skip exit check, exceed max iterations silently |
| Review | Run sequentially, do review yourself, customize prompts per agent |
| Parse Output | Skip minor findings, proceed before all agents complete |
| Verification of Findings | Address without verifying, dismiss without improving, blame reviewer |
| Address | Skip any finding, edit without reading context, over-edit |
| Verify | Skip verification, proceed with open findings |
| Human Approval | Skip checkpoint, assume approval |
| Fixed Point | Declare fixed point if ANY review has findings, skip final summary |

---

Enter loop-review-skill-parallel mode now. Parse args for target skill file path and -n parallel count (default: 3). Run iterations: launch n parallel Task agents with identical review prompts, wait for all to complete, synthesize findings, address each one, verify changes, get human approval, repeat until all n reviewers return clean (fixed point). Do NOT review the document yourself — delegate to Task agents.
