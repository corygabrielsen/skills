---
name: review-skill
description: Review a skill document using specialized lenses. Each lens finds specific issue types. Clean from all lenses = no findings.
---

# Review Skill

Review skill documents using specialized lenses. Each lens is tuned to find specific issue types with high signal and low noise.

## Lenses

Each lens asks a focused question. A finding from any lens is signal. Clean from all lenses means no findings.

| Lens | Question | Finds |
|------|----------|-------|
| execution | "Would this cause wrong behavior?" | Logic errors, missing steps, broken flows |
| checklist | "Do these specific checks pass?" | Structural issues, missing sections |
| contradictions | "Does A contradict B?" | Conflicting instructions |
| terminology | "Is term X used consistently?" | Naming inconsistencies |
| adversarial | "Can this be misinterpreted?" | Ambiguities, edge cases |
| gaps | "What's missing for each option/path?" | Unhandled branches, missing handlers |

---

## Phase: Initialize

### Do:
- Accept target skill file path from args
- Validate file exists and is a SKILL.md
- Read the full file content for lens prompts

### Don't:
- Start without a target file
- Review non-skill files

**Args:**
- First positional arg: path to SKILL.md (required)

---

## Phase: Fan Out

**Launch all lenses in parallel. Each lens gets a specialized prompt.**

### Do:
- Use `Task` tool with `run_in_background: true`
- Launch all 6 lenses in a **single message** (6 separate Task calls, one per lens)
- Store all 6 task IDs in a list for collection

### Don't:
- Run lenses sequentially
- Combine multiple lenses into one prompt
- Use identical prompts (each lens is specialized)

### Lens Prompts

**Lens: execution**
```
Review {target_file} for execution correctness.

Question: Would an LLM following this document do the wrong thing?

Assume standard Claude Code tools exist (Task, TaskOutput, Edit, AskUserQuestion, Bash, Read, etc.).

Only report issues where the answer is YES. Ignore:
- Stylistic preferences
- Minor wording variations
- Things that are clear from context
- Tool existence (assume standard tools work)

Output:
FINDINGS:
1. Line X: [what would go wrong]
...

OR

NO FINDINGS
```

**Lens: checklist**
```
Review {target_file} with this checklist.

Check ONLY:
1. Does each phase have Do and Don't sections?
2. Are status values used consistently throughout?
3. Are placeholder names in templates distinct (no reuse for different meanings)?
4. Do phase names match between any sequence lists and section headers?
5. Are tool parameters documented accurately?

Output:
FINDINGS:
1. [which check failed]: [details]
...

OR

NO FINDINGS - all checks pass.
```

**Lens: contradictions**
```
Review {target_file} for contradictions.

Find ONLY places where the document says two incompatible things.

NOT a contradiction:
- Same idea phrased differently
- Missing information
- Stylistic inconsistency

IS a contradiction:
- Section A says X, Section B says not-X
- A rule and an example that violate it

Output:
FINDINGS:
1. Lines X and Y contradict: [explanation]
...

OR

NO CONTRADICTIONS
```

**Lens: terminology**
```
Review {target_file} for terminology consistency.

For each key concept, verify the same term is used throughout.
Flag only cases where different terms are used for the SAME concept.

Do NOT flag:
- Different concepts that happen to have similar names
- Intentional distinctions (e.g., "phase" vs "section" if they mean different things)

Output:
FINDINGS:
1. "[term A]" vs "[term B]" for same concept: lines X, Y, Z
...

OR

NO FINDINGS - terminology is consistent.
```

**Lens: adversarial**
```
Review {target_file} adversarially.

Try to find ways to misinterpret this document that would lead to wrong behavior.

If you CAN'T find a plausible misinterpretation, the document is robust.

Output:
EXPLOITABLE AMBIGUITIES:
1. Line X: Could be read as "[bad interpretation]" leading to [wrong behavior]
...

OR

ROBUST - no exploitable ambiguities found.
```

**Lens: gaps**
```
Review {target_file} for missing handlers.

Look for places where options are presented but handling is incomplete:
- AskUserQuestion options without instructions for what to do when selected
- Conditional branches without all paths documented
- Edge cases mentioned but not handled

Output:
GAPS:
1. Line X: [option/branch] has no handling instructions
...

OR

NO GAPS - all paths handled.
```

---

## Phase: Collect

**Gather results from all lenses.**

### Do:
- Use `TaskOutput` for each lens
- Wait for all to complete
- Parse each lens's output format

### Don't:
- Proceed before all lenses complete
- Ignore any lens's findings

### Evaluate Results

```
if ALL lenses return clean (NO FINDINGS / NO CONTRADICTIONS / ROBUST / etc.):
    → Present "All lenses clean." and proceed to Epilogue
else:
    → Merge findings into tracker
    → Proceed to Synthesize
```

### Issue Tracker Format

```markdown
| ID | Lens | Line | Issue | Status |
|:--:|:----:|:----:|:------|:------:|
| F-001 | execution | 42 | [description] | open |
| F-002 | gaps | 156 | [description] | open |
```

---

## Phase: Synthesize

**Group findings by root cause, not by lens.**

A single root cause may be caught by multiple lenses. Group them.

### Do:
- Look for findings that point to the same underlying issue
- Name themes clearly (2-5 words)
- List truly unrelated findings separately

### Don't:
- Group by lens (lenses are detection methods, not categories)
- Force unrelated findings into themes

---

## Phase: Triage

**Propose resolutions. Don't edit yet.**

### Do:
- Propose ONE fix per theme
- Categorize: real issue, ambiguity, or missing content
- Update status to `planned`

### Don't:
- Make edits during triage
- Dismiss findings

---

## Phase: Plan Approval

**Present plan to user BEFORE making edits.**

### Do:
- Show themes and proposed fixes
- Use AskUserQuestion with clear options
- Wait for explicit approval

### Plan Approval Options

```
AskUserQuestion(
  questions: [{
    question: "Approve plan to address these findings?",
    header: "Plan",
    options: [
      {label: "Approve", description: "Proceed to make edits"},
      {label: "Modify", description: "I'll provide different approach"},
      {label: "Abort", description: "Do not make changes"}
    ],
    multiSelect: false
  }]
)
```

**If user selects "Modify":** Wait for user input, update plan accordingly, re-present for approval.

**If user selects "Abort":** End skill without changes.

---

## Phase: Address

**Execute the approved plan.**

### Do:
- Make edits using `Edit` tool
- Update tracker status (`planned` → `fixed`)
- Process each theme

### Don't:
- Deviate from approved plan
- Skip any finding

---

## Phase: Verify

**Confirm changes were made correctly.**

### Do:
- Re-read modified sections
- Check for unintended side effects
- Ensure all findings are `fixed`

### Don't:
- Skip verification
- Proceed with unaddressed findings

---

## Phase: Change Confirmation

**Get user confirmation of executed changes.**

### Do:
- Present summary of changes made
- Use AskUserQuestion with clear options
- Wait for explicit confirmation

### Don't:
- Skip this checkpoint
- Assume confirmation

### Confirmation Options

```
AskUserQuestion(
  questions: [{
    question: "Confirm changes look correct?",
    header: "Confirm",
    options: [
      {label: "Confirm", description: "Changes are good"},
      {label: "View diff", description: "Show git diff first"},
      {label: "Revert", description: "Undo changes"}
    ],
    multiSelect: false
  }]
)
```

**If user selects "View diff":** Run `git diff {target_file}`, show output, re-ask.

**If user selects "Revert":** Run `git checkout {target_file}`, confirm revert, end skill.

---

## Phase: Epilogue

**Report results and end.**

### Do:
- Report outcome (clean or findings addressed)
- End the skill

### Don't:
- Skip the completion message
- Continue after reporting

**All lenses clean:**
```
All lenses clean. No findings.
```

**Findings addressed:**
```
Review complete.
Findings: {count} addressed across {lens_count} lenses.
```

---

## Quick Reference

| Phase | Purpose |
|-------|---------|
| Initialize | Parse args, validate target |
| Fan Out | Launch all lenses in parallel |
| Collect | Gather and merge results |
| Synthesize | Group by root cause |
| Triage | Propose fixes |
| Plan Approval | Human checkpoint |
| Address | Make edits |
| Verify | Confirm changes |
| Change Confirmation | Human checkpoint |
| Epilogue | Report and end |

---

Begin /review-skill now. Parse args for target file. Launch all 6 lenses in parallel with their specialized prompts. If all return clean, report "All lenses clean." and end. Otherwise: synthesize findings into themes, triage, get Plan Approval, execute edits, verify, and get Change Confirmation.
