---
name: converge
description: Iterate observe→decide→act until target reached, actions exhausted, or iteration cap. Runs as a compiled CLI; agent invokes and interprets halt.
args:
  - name: fitness
    description: Fitness skill name (e.g. `pr-fitness`)
  - name: rest
    description: Args passed verbatim to the fitness skill
  - name: --max-iterations
    description: Default 20
  - name: --verbose
    description: Verbose logging
---

# /converge

Run the compiled converge CLI **alone in its own Bash call**. Do not
pipe, redirect, or combine with other commands — converge observes
everything it needs via the fitness skill. The **exit code** is the
communication channel; piping to `tail`, `head`, `grep`, etc.
swallows it.

```bash
npx tsx ~/code/skills/converge/src/cli.ts <fitness> <args...>
```

For `pr-fitness`, the repo can be omitted when running from inside
the repo checkout — converge infers it via `gh repo view`:

```bash
npx tsx ~/code/skills/converge/src/cli.ts pr-fitness <owner/repo> <pr>
npx tsx ~/code/skills/converge/src/cli.ts pr-fitness <pr>
```

After converge exits, read `/tmp/converge/{session-id}/exit.json` for
the structured halt report. Do not parse stderr.

## Halt taxonomy (by exit code)

| Exit | Status                |
| ---- | --------------------- |
| 0    | `success`             |
| 1    | `stalled`             |
| 2    | `timeout`             |
| 3    | `hil`                 |
| 4    | `error`               |
| 5    | `llm_needed`          |
| 6    | `pr_terminal`         |
| 7    | `cancelled`           |
| 8    | `fitness_unavailable` |
| 9    | `lock_held`           |

Final halt report at `/tmp/converge/{session-id}/exit.json`. The CLI
writes a `stage: "in_progress"` stub on startup and overwrites with
`stage: "final"` on halt — consumers check `stage` before reading
status details.

## Score

`score` is a numeric scalar, higher = better, emitted by the fitness
skill. `/converge` halts `success` iff `score >= target`. The fitness
skill defines what the scalar means; `/converge` never interprets it.
See the fitness skill's own SKILL.md for score semantics.

## Resume on `llm_needed` (exit 5)

1. Read `exit.json`: `status === "llm_needed"`, `action` has the LLM
   task, `resume_cmd` has the invocation to re-run
2. Delegate to sub-agent with `action.description` (and
   `action.context` if present)
3. Run `exit.json.resume_cmd` — session resumes from `history.jsonl`

The halt line on exit 5 prints the action and description, and a
following `to resume:` line emits the skill-form invocation verbatim.

## Compose

```
/timebox 90m /converge <fitness> <args>
/loop 30m /converge <fitness> <args>
```
