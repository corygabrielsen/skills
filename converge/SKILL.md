---
name: converge
description: Iterate observe→decide→act until target reached, actions exhausted, or iteration cap. Generic loop; slot in any fitness skill.
args:
  - name: fitness
    description: Skill name to invoke for observation
  - name: rest
    description: Args passed verbatim to the fitness skill
  - name: --target
    description: Target value; default per fitness skill
  - name: --max-iterations
    description: Default 20
---

# /converge

```
session = date +%s
for i in 1..max_iterations:
  report = invoke(fitness, args)
  save /tmp/converge-{session}/iter-{i}.json

  if target reached:                halt success
  if no actions:                    halt done

  action = report.actions[0]
  if action.automation == "human":  halt hil

  execute(action)                   # wait → sleep+continue; else act from description
halt timeout
```

On subagent/API failure: halt `error`.
On any halt: write `/tmp/converge-{session}/exit.json` with `{ status, iterations, final_fitness }`.

## Fitness contract

Structured output with:

- A scalar the target predicate reads
- `actions[]` ranked, each with `kind`, `description`, `automation ∈ {full, llm, wait, human}`

## Compose

```
/timebox 90m /converge <fitness> <args>
/loop 30m /converge <fitness> <args>
```
