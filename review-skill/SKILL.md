---
name: review-skill
description: Review a skill document using specialized reviewers. Each reviewer finds specific issue types.
---

# Review Skill

Review skill documents using specialized reviewers. Each reviewer is tuned to find specific issue types with high signal and low noise.

## Core Philosophy

**Every finding demands a document change. No exceptions.**

When a reviewer flags something, the document changes. Always. Either:
- **Real issue** → fix the document
- **False positive** → the document was unclear; add clarifying text until the intent is obvious

There is no "dismiss," no "already documented," no "tool behavior." If a reviewer misunderstood, that's a signal the document isn't self-evident—another LLM would misunderstand too. The document must become clearer.

**Fixed point** = no reviewer can find *anything* to flag. Not because you argued them down, but because the document is both **correct** AND **unambiguous**.

---

## Reviewers

Each reviewer asks a focused question. An issue from any reviewer is signal.

| Reviewer | Question | Finds |
|------|----------|-------|
| execution | "Would this cause wrong behavior?" | Logic errors, missing steps, broken flows |
| checklist | "Do these specific checks pass?" | Structural issues, missing sections |
| contradictions | "Does A contradict B?" | Conflicting instructions |
| terminology | "Is term X used consistently?" | Naming inconsistencies |
| adversarial | "Where would a reasonable LLM go wrong?" | Fixable ambiguities, missing info |
| coverage | "Is every option/branch handled?" | Unhandled branches, missing handlers |

---

## Phase: Initialize

### Do:
- Accept target skill file path from args
- Validate file exists and has basename `SKILL.md`
- Read the full file content for reviewer prompts
- Store `target_file` path in working memory for substitution into prompts and commands

### Don't:
- Start without a target file
- Review non-skill files

**Args:**
- First positional arg: path to SKILL.md (required)

**If validation fails:** Report error and end skill.

---

## Phase: Fan Out

**Launch all reviewers in parallel. Each reviewer gets a specialized prompt.**

### Do:
- Use `Task` tool with `run_in_background: true` and `prompt: <reviewer prompt>`
- Launch all 6 reviewers in a **single assistant turn** (one message containing 6 parallel Task tool calls)
- Store all 6 task IDs (from tool response) for collection—tool results are returned in the same order as tool calls, so track reviewer by position: (1) execution, (2) checklist, (3) contradictions, (4) terminology, (5) adversarial, (6) coverage
- Verify 6 task IDs were returned; if fewer, the result at that position contains an error message instead of a task ID—record "Reviewer [name] failed to launch: [error]" as an issue

### Don't:
- Run reviewers sequentially
- Combine multiple reviewers into one prompt
- Use identical prompts (each reviewer is specialized)

### Reviewer Prompts

**Reviewer: execution**
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
ISSUES:
1. Line X: [what would go wrong]
...

OR

NO ISSUES
```

**Reviewer: checklist**
```
Review {target_file} with this checklist.

Check ONLY:
1. Does each phase have Do and Don't sections?
2. Are status values used consistently throughout?
3. Are placeholder names in templates distinct (no reuse for different meanings)?
4. Do phase names match between any sequence lists and section headers?
5. Are tool parameters documented accurately?

Output:
ISSUES:
1. [which check failed]: [details]
...

OR

NO ISSUES
```

**Reviewer: contradictions**
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
ISSUES:
1. Lines X and Y contradict: [explanation]
...

OR

NO ISSUES
```

**Reviewer: terminology**
```
Review {target_file} for terminology consistency.

For each key concept, verify the same term is used throughout.
Flag only cases where different terms are used for the SAME concept.

Do NOT flag:
- Different concepts that happen to have similar names
- Intentional distinctions (e.g., "phase" vs "section" if they mean different things)

Output:
ISSUES:
1. "[term A]" vs "[term B]" for same concept: lines X, Y, Z
...

OR

NO ISSUES
```

**Reviewer: adversarial**
```
Review {target_file} adversarially.

Imagine a less-capable LLM or hurried reader following this document. Find places where they would go wrong.

Focus on issues FIXABLE by improving the document:
- Ambiguous instructions with multiple valid interpretations
- Missing information needed to choose the right action
- Implicit assumptions that should be explicit
- Easy-to-miss qualifiers or conditions

Do NOT flag issues outside the document's control:
- Tool behavior (assume standard tools work correctly—Task returns IDs in order, TaskOutput blocks, Edit produces unstaged changes)
- User actions outside the skill's flow
- Environment variations
- Misreadings that require ignoring explicit statements in surrounding context
- Concerns already addressed by explicit instructions elsewhere in the document

Before flagging, ask: "What edit to this document would fix this?"
If you can't answer, don't flag it.

Output:
ISSUES:
1. Line X: A reasonable LLM would [wrong behavior] because [why context doesn't resolve it], fixable by [specific edit]
...

OR

NO ISSUES
```

**Reviewer: coverage**
```
Review {target_file} for complete handler coverage.

Look for places where options are presented but handling is incomplete:
- AskUserQuestion options without instructions for what to do when selected
- Conditional branches without all paths documented
- Edge cases mentioned but not handled

Output:
ISSUES:
1. Line X: [option/branch] has no handling instructions
...

OR

NO ISSUES
```

---

## Phase: Collect

**Gather results from all reviewers.**

### Do:
- Use `TaskOutput` with `task_id: <id>` for each reviewer **in a single turn (6 parallel calls)** to wait for completion
- Parse each reviewer's output format

### Don't:
- Proceed before all reviewers complete
- Ignore any reviewer's issues

### Evaluate Results

A reviewer has no issues if its output contains `NO ISSUES`. Treat malformed output (neither "NO ISSUES" nor valid "ISSUES: 1. Line X..." format) or failed reviewer output (task execution error) as having issues—record "Reviewer failed: [error]" in the Issue field (use "-" for Line column) and follow the normal issue path (proceed to Synthesize).

```
if ALL 6 reviewers output NO ISSUES:
    → Proceed to Epilogue (no-issues path)
else:
    → Merge issues into tracker
    → Proceed to Synthesize
```

### Issue Tracker Format

```markdown
| ID | Reviewer | Line | Issue | Status |
|:--:|:--------:|:----:|:------|:------:|
| I-001 | execution | 42 | [description] | open |
| I-002 | coverage | 156 | [description] | open |
```

**Status progression:** `open` → `planned` (in Triage) → `fixed` or `clarified` (in Address)

- `fixed` = real issue was corrected
- `clarified` = false positive addressed by adding clarifying text

---

## Phase: Synthesize

**Group issues by root cause, not by reviewer.**

A single root cause may be caught by multiple reviewers. Group them.

### Do:
- Look for issues that point to the same underlying issue (e.g., if terminology flags "X vs Y" inconsistency and execution flags wrong behavior caused by that naming, they share root cause "inconsistent naming")
- Name themes clearly (2-5 words)
- List truly unrelated issues separately

### Don't:
- Group by reviewer (reviewers are detection methods, not categories)
- Force unrelated issues into themes

---

## Phase: Triage

**Propose fixes. Don't edit yet.**

### Do:
- Propose ONE fix per theme
- For each issue, determine resolution type:
  - **Real issue** → propose document fix
  - **False positive** → propose clarifying text (comment, note, or rewording that makes intent obvious)
- Immediately after proposing a fix for a theme, update that theme's issues from `open` to `planned`

### Don't:
- Make edits during triage
- Dismiss issues as "not actionable" or "tool behavior"—every finding gets a document change
- Skip issues because they seem minor—if a reviewer flagged it, the document can be clearer

---

## Phase: Plan Approval

**Present plan to user BEFORE making edits.**

### Do:
- Show themes and proposed fixes
- Use AskUserQuestion with clear options
- Wait for explicit approval

### Don't:
- Make edits before approval
- Skip this checkpoint

### Plan Approval Options

```
AskUserQuestion(
  questions: [{
    question: "Approve plan to address these issues?",
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

**If user selects "Approve":** Proceed to Address phase.

**If user selects "Modify":**
1. Acknowledge selection and prompt: "Please describe your changes."
2. End turn (stop responding and wait for user input).
3. When user provides input, update plan accordingly.
4. Show updated plan to user (same format as original Triage output).
5. Re-present Plan Approval options (repeat until user selects Approve or Abort).

**If user selects "Abort":** End skill without changes.

---

## Phase: Address

**Execute the approved plan.**

### Do:
- Process each theme: make one or more edits as needed
- Update tracker status based on resolution type:
  - `planned` → `fixed` (real issue was corrected)
  - `planned` → `clarified` (false positive addressed with clarifying text)
- Use `Edit` tool for changes

### Don't:
- Deviate from approved plan
- Skip any issue
- Leave any issue without a document change

---

## Phase: Verify

**Confirm changes were made correctly.**

### Do:
- Use Read tool on `{target_file}` to verify changes appear correctly in context
- Check that surrounding text still makes sense with the edits
- Ensure all issues are `fixed` or `clarified`

### Don't:
- Skip verification
- Proceed with unaddressed issues

---

## Phase: Change Confirmation

**Get user confirmation of executed changes.**

### Do:
- Present summary: issue tracker showing final statuses, plus brief prose summary of key changes
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

**If user selects "Confirm":** Proceed to Epilogue.

**If user selects "View diff":**
1. Run `git diff {target_file}` to show unstaged changes (Edit tool produces unstaged changes).
2. Show the diff output to user.
3. Handle edge cases:
   - If diff is empty: report "No unstaged changes to show."
   - If file is untracked: report "File is untracked (not yet committed)."
4. Re-present confirmation options.

**If user selects "Revert":**
1. Warn user: "This will discard unstaged changes to {target_file}. Staged changes are not affected—use `git restore --staged {target_file}` first if needed."
2. Run `git checkout -- {target_file}` to restore the file.
3. Handle edge cases:
   - Success: report "Changes reverted." and end skill.
   - File never committed (git errors "pathspec did not match"): report this error and end skill.
   - Note: If changes were staged, `git checkout --` has no effect (file already matches staged version). The warning in step 1 informs user of this.

---

## Phase: Epilogue

**Report results and end.**

### Do:
- Report outcome
- End the skill

### Don't:
- Skip the completion message
- Continue after reporting

**No issues found:**
```
No issues.
```

**Issues addressed:**
```
Review complete.
Issues: {fixed_count} fixed, {clarified_count} clarified (from {reviewers_with_issues} reviewers).
```

- `{fixed_count}` = issues where real problems were corrected
- `{clarified_count}` = false positives addressed by adding clarifying text
- `{reviewers_with_issues}` = count of reviewers that reported at least one issue

---

## Quick Reference

| Phase | Purpose |
|-------|---------|
| Initialize | Parse args, validate target |
| Fan Out | Launch all reviewers in parallel |
| Collect | Gather and merge results |
| Synthesize | Group by root cause |
| Triage | Propose fixes |
| Plan Approval | Human checkpoint |
| Address | Make edits |
| Verify | Confirm changes |
| Change Confirmation | Human checkpoint |
| Epilogue | Report and end |

---

Begin /review-skill now. Parse args for target file. Launch all 6 reviewers in parallel with their specialized prompts. Collect results. Follow phase flow based on results: if all reviewers output NO ISSUES, skip to Epilogue; otherwise continue Synthesize → Triage → Plan Approval → Address → Verify → Change Confirmation → Epilogue.
