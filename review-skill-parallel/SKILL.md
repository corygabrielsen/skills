---
name: review-skill-parallel
description: Single iteration of skill document review with n parallel reviewers. All reviewers are fungible (same prompt). Synthesizes findings, addresses issues, verifies changes, gets human approvals (plan and confirmation). Use /loop-review-skill-parallel for iterating until fixed point.
---

# Review Skill (Parallel)

You are a skill document reviewer. **Task agents serve as reviewers — you address their findings.** Multiple identical reviewers catch different issues through execution diversity.

**Terminology note**: "Task agents" refers to the technical mechanism (agents launched via the `Task` tool). "Reviewers" refers to their conceptual role. The document uses "Task agents" when discussing implementation details and "reviewers" when discussing the conceptual workflow.

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
│  n Task agents  │────▶│  You address      │
│  (fungible)     │     │                   │
└─────────────────┘     └───────────────────┘
```

This diagram is conceptual — the full phase sequence is: Initialize → Review → Parse Output → Synthesize → Triage → Plan Approval → Address → Verify → Change Confirmation → Epilogue.

You address the reviewers' findings through the phases below.

## Relationship to loop-review-skill-parallel

| Aspect | review-skill-parallel | loop-review-skill-parallel |
|--------|----------------------|----------------------------|
| **Scope** | Single iteration | Iterate until fixed point |
| **Output** | Findings addressed, changes confirmed | Document at fixed point |
| **When** | Called by loop wrapper or standalone | User invokes directly |

Use **/loop-review-skill-parallel** to iterate this skill until all n reviewers return clean.

## State Schema

Track during this iteration:

```yaml
target_file: ""              # Path to skill file being edited
parallel_review_count: 3     # -n flag (default 3)
task_ids: []                 # Task IDs for result collection
# issue_tracker is maintained as a markdown table (see Parse Output phase)
```

The issue tracker is conceptual — maintained in your working context during the iteration, not persisted to disk.

### Tools Assumed

This skill uses standard Claude Code tools without detailed explanation:
- `Task` — Launch background agents; returns task_id (`description`, `prompt`, `subagent_type`, `run_in_background`)
- `TaskOutput` — Retrieve agent results (`task_id`, `block`, `timeout`)
- `Edit` — Modify files (`file_path`, `old_string`, `new_string`)
- `AskUserQuestion` — Present options to user (`questions` array containing objects with `question`, `header`, `options`, `multiSelect`)

---

## Phase: Initialize

### Do:
- Accept target skill file path from args
- Validate file exists and is a SKILL.md
- Initialize state

### Don't:
- ❌ Start without a target file — require explicit path
- ❌ Review non-skill files — this skill is for SKILL.md files only

**On activation:**

1. Parse args for target file:
   ```
   /review-skill-parallel path/to/SKILL.md           # Review with 3 parallel reviewers
   /review-skill-parallel path/to/SKILL.md -n 5      # 5 parallel reviewers
   ```

2. Validate target exists and contains YAML frontmatter with `name:` field

3. Initialize state

**Args:**
- First positional arg: path to SKILL.md (required)
- `-n <count>`: number of parallel reviewers (default: 3)

---

## Phase: Review

**Launch n parallel Task agents to review the skill document. All agents are fungible — same prompt.**

### Do:
- Use `Task` tool with `run_in_background: true`
- Launch all n Task agents in a **single message** (parallel)
- Use identical prompt for all agents
- Record all task IDs for result collection

### Don't:
- ❌ Run reviews sequentially — always parallel
- ❌ Do the review yourself — delegate to Task agents
- ❌ Customize prompts per agent — all reviewers are fungible

### Review Prompt Template

All agents receive the same prompt:

```
You are reviewing {target_file} for internal consistency and clarity issues.

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
  description: "Review {target_file} (1/3)",
  prompt: "[review prompt with {target_file} substituted]",
  subagent_type: "general-purpose",
  run_in_background: true
)
Task(
  description: "Review {target_file} (2/3)",
  prompt: "[same review prompt]",
  subagent_type: "general-purpose",
  run_in_background: true
)
Task(
  description: "Review {target_file} (3/3)",
  prompt: "[same review prompt]",
  subagent_type: "general-purpose",
  run_in_background: true
)
```

Each Task returns a task ID for use in Parse Output.

---

## Phase: Parse Output

**Collect results from all n reviewers and merge into the issue tracker.**

### Do:
- Use `TaskOutput` tool to collect results from each Task agent:
  ```
  TaskOutput(task_id: "task_id_here", block: true, timeout: 120000)
  ```
- Extract findings from each reviewer's output
- Merge into issue tracker, deduplicating similar findings (same line + similar description = one finding)
- Record which reviewers found each issue

**Clean iteration = all n reviewers return "NO FINDINGS".** If ANY review has findings, proceed to Synthesize.

### Don't:
- ❌ Skip findings because they seem minor — every finding gets tracked
- ❌ Proceed before all reviewers complete — wait for all n

### Evaluate n Parallel Results

```
results = [reviewer_1, reviewer_2, ..., reviewer_n]

if ALL n results are "NO FINDINGS":
    → Iteration is clean — proceed to Change Confirmation ("Clean iteration — no findings, no changes.")
else:
    # ANY reviewer has findings
    → Merge all findings into tracker
    → Proceed to Synthesize phase
```

### Issue Tracker Format

```markdown
| ID | Line | Issue | Status | Reviewers |
|:--:|:----:|:-----------|:------:|:---------:|
| F-001 | 42 | Terminology: "bug" vs "finding" inconsistent | open | 1,3 |
| F-002 | 108 | Missing Do/Don't section in Phase: Verify | open | 2 |
```

The "Reviewers" column shows which of the n reviewers (numbered 1 through n) flagged this issue.

Statuses:
- `open` — finding identified, not yet addressed
- `planned` — resolution proposed, awaiting human approval
- `fixed` — real inconsistency corrected
- `clarified` — wording improved or rationale documented to prevent future misunderstanding (for false positives or design tradeoffs)

---

## Phase: Synthesize

**Zoom out. Understand the document as a system before addressing any finding.**

⚠️ **This step is not optional, and it's not just for "complex" findings.**

Skill documents have interconnected sections, implicit contracts between phases, and terminology that must be consistent throughout. A finding that looks like a simple wording fix often touches deeper structural issues.

### The Protocol

1. **Read the full context** — Not just the flagged line. Read the entire section, the sections it references, and the sections that reference it. The finding is a pointer; the truth is in the document structure.

2. **Map the system** — Trace the relevant connections:
   - What phases reference this concept?
   - What terminology chains exist (does "agent" here connect to "reviewer" elsewhere)?
   - What implicit contracts exist between sections?

3. **Look for patterns** — Findings in the same area or touching the same concept may share a root cause. A single finding may reveal a pattern repeated elsewhere.

4. **Ask the hard questions:**
   - What contract should this section uphold?
   - Does every reference honor that contract?
   - What would a surface-level fix miss?
   - Is there a structural issue underneath?

5. **Challenge yourself** — "Is this my best effort? What haven't I considered?"

### Group by Theme

After understanding the system, organize findings for human review:

- Review all findings together as a set
- Identify themes and patterns (e.g., "terminology inconsistency" appears in 8 findings)
- Group findings by root cause
- Name each theme clearly (2-5 words)
- Aim for 3-7 themes, not 15 — if you have too many, you haven't found the root causes

### Do:
- Understand the document structure before grouping
- Map how sections interconnect
- Find root causes, not just surface patterns
- Note how many findings each theme covers

### Don't:
- ❌ Skip straight to triaging findings one-by-one — always synthesize first
- ❌ Group mechanically without understanding — themes should reflect *why* findings exist
- ❌ Force unrelated findings into themes — list them individually instead

### Common Theme Patterns

- **Terminology inconsistency**: Same concept, different names (commonly the largest category)
- **Structural inconsistency**: Missing sections, formatting variations
- **Flow/reference errors**: Wrong phase names, outdated cross-references
- **Contract violations**: Section promises something another section doesn't deliver
- **Scope bleed**: Content that belongs in a different skill/phase
- **Redundancy**: Same information repeated with slight variations

### Theme Summary Format

```markdown
## Synthesis: {finding_count} findings → {theme_count} themes

| Theme | Findings | Root cause |
|-------|----------|------------|
| Terminology: "agent" vs "reviewer" | F-001, F-004, F-007, F-012 | No single term chosen |
| Phase reference errors | F-002, F-008 | Phases renamed but refs not updated |
| Missing status transitions | F-003, F-005, F-006 | Status flow never documented |

**Unrelated findings** (no shared root cause):
- F-009: [individual description and fix]
- F-010: [individual description and fix]
- F-011: [individual description and fix]
```

Addressing one theme often resolves multiple findings simultaneously. Understanding *why* the theme exists prevents incomplete fixes.

---

## Phase: Triage

**Propose resolutions by theme, not by individual finding. Don't make edits yet.**

Work through themes identified in Synthesize. For each theme, propose one root-cause fix that resolves all findings in that group.

### Do:
- Work theme-by-theme from Synthesize output
- Read context around each theme's findings
- Propose ONE resolution per theme (not per finding)
- Categorize: real inconsistency, false positive, or design tradeoff
- Update all findings in theme to `planned` status

### Don't:
- ❌ Make edits during triage — propose only
- ❌ Dismiss findings — every finding gets a proposed resolution
- ❌ Triage findings within a theme one-by-one — work by theme (unrelated findings are handled individually)
- ❌ Blame the reviewer — if an LLM got confused, another will too

### Triage Table

| Finding Type | Resolution Type | Final Status (after Address) |
|--------------|-----------------|------------------------------|
| Real inconsistency | Fix the document | `fixed` |
| False positive | Rewrite until intent is obvious | `clarified` |
| Design tradeoff | Document rationale explicitly | `clarified` |

Triage changes status from `open` → `planned`. Address phase changes `planned` → final status (`fixed` or `clarified`).

---

## Phase: Plan Approval

**Present findings and proposed resolutions to user BEFORE making any edits.**

This is the first human-in-the-loop checkpoint. The user can:
- Approve the plan and proceed to edits
- Modify proposed resolutions
- Add context or requirements
- Request different approaches

### Do:
- Present executive summary with findings and proposed resolutions
- Explain the reasoning behind each proposed resolution
- Use `AskUserQuestion` tool with clear options
- Wait for explicit approval before any edits

### Don't:
- ❌ Make edits before approval — this is a PLAN checkpoint
- ❌ Skip this checkpoint — human input is critical before changes
- ❌ Assume approval — wait for explicit response

### Plan Summary Template

Transform the Theme Summary Format from Synthesize into a plan by adding proposed fixes for each theme. Present by theme, not by individual finding. This makes review tractable for humans.

```markdown
## Review Findings: {finding_count} findings in {theme_count} themes

### Theme 1: Terminology — "agent" vs "reviewer" (4 findings)

**Root cause**: Document uses both terms interchangeably.

**Findings**: F-001, F-004, F-007, F-012

**Proposed fix**: Standardize on "reviewer" throughout. One search-and-replace resolves all 4.

---

### Theme 2: Phase reference errors (2 findings)

**Root cause**: Phases were renamed but cross-references not updated.

**Findings**: F-002, F-008

**Proposed fix**: Update all references to match current phase names (e.g., "Approval" → "Plan Approval").

---

### Unrelated findings (3 findings)

These have no shared root cause; list individually:

**F-003** (line 299): "code sections" should be "document sections"
- Fix: Change "code" to "document"

**F-005** (line 452): Missing example for clean iteration
- Fix: Add clean iteration example to Change Confirmation

**F-006** (line 437): "approves" vs "confirms" inconsistency
- Fix: Change to "confirms"

---

### Summary
- 3 themes covering 9 findings
- 2 root-cause fixes resolve 6 findings
- 3 standalone point fixes
```

### Plan Approval Options

```
AskUserQuestion(
  questions: [{
    question: "Approve plan to address these findings?",
    header: "Plan",
    options: [
      {label: "Approve plan", description: "Proceed to make edits"},
      {label: "Modify plan", description: "I'll provide different approach"},
      {label: "Need more context", description: "Show me the relevant document sections"},
      {label: "Abort", description: "Do not make any changes"}
    ],
    multiSelect: false
  }]
)
```

---

## Phase: Address

**Execute the approved plan. Make edits to resolve all findings.**

### Do:
- Address all planned findings from the tracker
- Use Edit tool for targeted changes
- Update tracker status as you go

### Don't:
- ❌ Deviate from approved plan — execute what was approved
- ❌ Skip any finding — every approved resolution must be executed
- ❌ Make changes without reading the relevant sections first
- ❌ Over-edit — make minimal changes to resolve each finding

### Address Protocol

For each theme (or individual unrelated finding):

1. **Read context** — Read the section(s) containing the finding
2. **Identify resolution** — Fix, clarify, or document rationale
3. **Make edit** — Use Edit tool with precise old_string/new_string
4. **Update tracker** — Mark as `fixed` or `clarified`

### Example: Addressing a Finding

```
Finding F-001: Line 42 uses "bug" but line 108 uses "finding"

Resolution: Standardize on "finding" throughout

Edit(
  file_path: "/path/to/SKILL.md",
  old_string: "every bug demands",
  new_string: "every finding demands"
)

Update tracker: F-001 status → fixed
```

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
- ❌ Proceed with unaddressed findings — all must be resolved

### Verification Checklist

```
[ ] All findings in tracker are `fixed` or `clarified`
[ ] Re-read each modified section
[ ] No new inconsistencies introduced by edits
[ ] Document still parses correctly (YAML frontmatter valid)
```

---

## Phase: Change Confirmation

**Present executed changes to user and get explicit confirmation.**

This is the second human-in-the-loop checkpoint. The user confirms the changes were executed correctly.

**Clean iteration (0 findings)**: If all reviewers returned "NO FINDINGS," present a minimal confirmation: "Clean iteration — no findings, no changes."

### Do:
- Present summary of changes made (not proposed — actually executed)
- Show which findings were resolved and how
- Use `AskUserQuestion` tool with clear options
- Wait for explicit confirmation

### Don't:
- ❌ Skip this checkpoint — human confirmation is mandatory
- ❌ Assume confirmation — wait for explicit response

### Change Summary Template

```markdown
## Changes Executed

### Findings Addressed: {finding_count}

| ID | Line | Issue | Resolution Applied |
|----|------|-------|-------------------|
| F-001 | 42 | "bug" vs "finding" inconsistency | Changed to "finding" |
| F-002 | 108 | Missing Do/Don't section | Added Do/Don't section |

### Edits Made
1. Line 42: Changed "bug" → "finding"
2. Lines 108-115: Added Do/Don't section

### Verification
- [x] All planned resolutions executed
- [x] Modified sections re-read
- [x] No new inconsistencies introduced
```

### Confirmation Options

```
AskUserQuestion(
  questions: [{
    question: "Confirm changes were executed correctly?",
    header: "Confirm",
    options: [
      {label: "Confirm", description: "Changes look correct, complete iteration"},
      {label: "View diff", description: "Show git diff (requires git), then re-ask"},
      {label: "Revert", description: "Something went wrong, undo changes"},
      {label: "Modify", description: "Need additional changes"}
    ],
    multiSelect: false
  }]
)
```

---

## Epilogue: Iteration Complete

When user confirms:

1. Report iteration results:
   ```
   Review complete.
   Findings: {finding_count} addressed
   Status: Iteration complete
   ```

2. End the skill. If called by /loop-review-skill-parallel, the loop wrapper will launch a new iteration as needed.

---

## Quick Reference: Don'ts

| Phase | Don't |
|-------|-------|
| Initialize | Start without target file, review non-skill files |
| Review | Run sequentially, do the review yourself, customize prompts per reviewer |
| Parse Output | Skip minor findings, proceed before all reviewers complete |
| Synthesize | Skip straight to triaging findings one-by-one, group mechanically without understanding, force unrelated findings into themes |
| Triage | Make edits during triage, dismiss findings, triage findings within a theme one-by-one, blame reviewer |
| Plan Approval | Make edits before approval, skip checkpoint, assume approval |
| Address | Deviate from plan, skip findings, edit without reading context, over-edit |
| Verify | Skip verification, proceed with unaddressed findings |
| Change Confirmation | Skip checkpoint, assume confirmation |

---

Begin review-skill-parallel now. Parse args for target skill file path and -n flag (default: 3 reviewers). Launch n parallel Task agents with identical review prompts. Wait for all to complete, synthesize findings into themes, triage by theme (unrelated findings are handled individually), get Plan Approval from user, execute the approved plan in Address, verify changes, and get Change Confirmation.
