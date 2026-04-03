# pr-fitness

Live PR merge readiness assessment. Queries GitHub APIs for every dimension that affects whether a PR can merge, returns structured JSON.

All state is queried fresh — nothing is cached, nothing is inferred from prior runs.

## Usage

```bash
# Quick check
npx tsx src/cli.ts example/widgets 1563

# Quiet mode (no stderr progress)
npx tsx src/cli.ts -q example/widgets 1563

# Pipe to jq
npx tsx src/cli.ts -q example/widgets 1563 | jq '.blockers'

# One-line summary (no JSON)
npx tsx src/cli.ts -q -s example/widgets 1563
# → "Blocked: ci_fail: Lint, not_approved"

# After npm run build
node dist/cli.js example/widgets 1563
```

## Output

```json
{
  "pr": 1563,
  "url": "https://github.com/example/widgets/pull/1563",
  "title": "Fix consensus correctness",
  "head": "3634facc",
  "base": "master",
  "lifecycle": "open",
  "merged_at": null,
  "closed_at": null,
  "mergeable": false,
  "blockers": ["not_approved"],
  "ci": { "pass": 19, "fail": 0, "pending": 0, "total": 19 },
  "reviews": { "decision": "REVIEW_REQUIRED", "threads_unresolved": 0 },
  "state": {
    "draft": false,
    "conflict": "MERGEABLE",
    "updated_at": "...",
    "last_commit_at": "..."
  }
}
```

### Key fields

| Field                        | Type       | Description                                                      |
| ---------------------------- | ---------- | ---------------------------------------------------------------- |
| `summary`                    | `string`   | Human-readable one-liner (e.g. "Blocked: ci_fail, not_approved") |
| `lifecycle`                  | `string`   | `open`, `merged`, or `closed`                                    |
| `merged_at`                  | `string?`  | ISO 8601 — when the PR was merged                                |
| `closed_at`                  | `string?`  | ISO 8601 — when the PR was closed                                |
| `mergeable`                  | `boolean`  | `true` when all hard blockers are clear (always true if merged)  |
| `blockers`                   | `string[]` | Human-readable list of blocking issues                           |
| `ci.fail`                    | `number`   | Number of failed CI checks                                       |
| `ci.failed`                  | `string[]` | Names of failed checks                                           |
| `ci.failed_details`          | `object[]` | `{name, description, link}` for each failed check                |
| `ci.completed_at`            | `string?`  | ISO 8601 — most recent check completion time                     |
| `reviews.decision`           | `string`   | `APPROVED`, `REVIEW_REQUIRED`, `CHANGES_REQUESTED`, or `NONE`    |
| `reviews.threads_unresolved` | `number`   | Unresolved review threads                                        |
| `state.draft`                | `boolean`  | PR is in draft mode                                              |
| `state.conflict`             | `string`   | `MERGEABLE`, `CONFLICTING`, or `UNKNOWN`                         |
| `state.updated_at`           | `string`   | ISO 8601 — last PR update (push, comment, label)                 |
| `state.last_commit_at`       | `string?`  | ISO 8601 — when HEAD commit was authored                         |

## Blockers

Hard blockers prevent merge. The `blockers` array lists all active ones:

| Blocker                | Meaning                                 |
| ---------------------- | --------------------------------------- |
| `ci_fail: <names>`     | CI checks failed                        |
| `ci_pending: <names>`  | CI checks still running                 |
| `stack_blocked`        | Downstack PR must merge first           |
| `not_approved`         | No review approval (policy requires it) |
| `N_unresolved_threads` | Unresolved review threads               |
| `merge_conflict`       | Conflicts with base branch              |
| `draft`                | PR is in draft mode                     |
| `wip_label`            | "work in progress" label present        |
| `title_too_long`       | Title + PR suffix exceeds 50 chars      |

## Exit codes (`-e` flag)

| Code | Meaning             |
| ---- | ------------------- |
| 0    | Open and mergeable  |
| 1    | Open but blocked    |
| 2    | Already merged      |
| 3    | Closed (not merged) |

## Development

```bash
npm install
npm run dev              # run via tsx (no build)
npm run typecheck        # tsc --noEmit
npm run lint             # eslint + prettier
npm run format           # prettier --write
npm run test             # unit tests
npm run build            # produce dist/
```

## Architecture

```
src/
  cli.ts              # entry: args, errors, stdout
  pr-fitness.ts       # orchestrator: collect → compute → report
  version.ts          # single source of truth for version
  collectors/         # parallel GitHub API calls (6 calls, ~2s)
  compute/            # pure functions: blockers, ci, plan, reviews, state
  types/              # input (raw API) and output (report contract)
  util/               # gh subprocess wrapper, logging, errors
test/
  fixtures/helpers.ts # shared test fixtures (makePr, CLEAN_CI, etc.)
  fixtures/pr-*.json  # captured output snapshots
  compute/*.test.ts   # unit tests for pure compute functions
  snapshot.test.ts    # output contract validation
```

The `compute/` layer is pure — no I/O, no subprocesses. Feed it typed data, get typed results. Trivially unit-testable.

The `collectors/` layer wraps `gh` CLI calls. Mock `util/gh.ts` in tests to make everything deterministic.

## Dependencies

**Runtime**: zero. Uses `gh` CLI (must be installed and authenticated) and Node built-ins.

**Dev**: typescript, tsx, eslint, prettier.
