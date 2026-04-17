#!/usr/bin/env node

import { DEFAULT_TARGET, prFitness } from "./pr-fitness.js";
import {
  PullRequestNumberFromString,
  RepoSlug,
  Score,
} from "./types/branded.js";
import type { Score as ScoreT } from "./types/branded.js";
import { PreconditionError } from "./util/errors.js";
import { setQuiet } from "./util/log.js";
import { VERSION } from "./version.js";

function usage(): never {
  process.stdout.write(`Usage: pr-fitness [options] <owner/repo> <pr_number>

Live PR merge readiness assessment. Queries all state fresh.

Options:
  -h, --help          Show this help
  -v, --version       Show version
  -q, --quiet         Suppress stderr progress
  -c, --compact       Compact JSON (single line)
  -s, --summary       Print one-line summary instead of JSON
  -e, --exit-code     Exit code reflects state (for scripts)
                        0 = open + mergeable
                        1 = open + blocked
                        2 = already merged
                        3 = closed (not merged)
      --target=<t>    Target score. Integer or tier label:
                        1  🥉 (bronze)
                        2  🥈 (silver)
                        3  🥇 (gold)
                        4  💠 (platinum)   ← default

Examples:
  pr-fitness example/widgets 1563
  pr-fitness -c example/widgets 1563
  pr-fitness -q example/widgets 1563 | jq '.blockers'
  pr-fitness --target=gold example/widgets 1563

Output fields:
  .score           current fitness scalar (0..4 for PRs)
  .target          target score the caller asked for
  .lifecycle       open, merged, or closed
  .terminal        present iff PR can no longer progress (merged/closed)
  .mergeable       true if all hard blockers clear
  .blockers[]      list of blocking issues (empty when mergeable)
  .ci              check pass/fail/pending counts and names
  .reviews         approval state, threads, bot comments
  .state           draft, labels, title, timestamps, assignees
  .actions[]       ordered plan to increase fitness (each has target_effect)
  .duration_ms     time to generate report
`);
  process.exit(0);
}

function die(message: string): never {
  process.stderr.write(`pr-fitness: ${message}\n`);
  process.exit(1);
}

const TIER_SCORES: Readonly<Record<string, number>> = {
  bronze: 1,
  silver: 2,
  gold: 3,
  platinum: 4,
};

function parseTarget(raw: string): ScoreT {
  if (/^[0-9]+$/.test(raw)) {
    return Score(Number(raw));
  }
  const mapped = TIER_SCORES[raw.toLowerCase()];
  if (mapped === undefined) {
    die(
      `invalid --target: ${raw} (expected integer or one of bronze|silver|gold|platinum)`,
    );
  }
  return Score(mapped);
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const positional: string[] = [];
  let compact = false;
  let summaryOnly = false;
  let exitCode = false;
  let target: ScoreT = DEFAULT_TARGET;

  for (const arg of args) {
    if (arg === "-h" || arg === "--help") usage();
    if (arg === "-v" || arg === "--version") {
      process.stdout.write(`pr-fitness ${VERSION}\n`);
      process.exit(0);
    }
    if (arg === "-q" || arg === "--quiet") {
      setQuiet(true);
    } else if (arg === "-c" || arg === "--compact") {
      compact = true;
    } else if (arg === "-s" || arg === "--summary") {
      summaryOnly = true;
    } else if (arg === "-e" || arg === "--exit-code") {
      exitCode = true;
    } else if (arg.startsWith("--target=")) {
      target = parseTarget(arg.slice("--target=".length));
    } else if (arg.startsWith("-")) {
      die(`unknown option: ${arg}`);
    } else {
      positional.push(arg);
    }
  }

  const rawRepo = positional[0];
  const rawPr = positional[1];

  if (!rawRepo) die("missing argument: <owner/repo>");
  if (!rawPr) die("missing argument: <pr_number>");

  const repo = RepoSlug(rawRepo);
  const pr = PullRequestNumberFromString(rawPr);

  const report = await prFitness(repo, pr, target);

  if (summaryOnly) {
    process.stdout.write(report.status + "\n");
  } else {
    const indent = compact ? undefined : 2;
    process.stdout.write(JSON.stringify(report, null, indent) + "\n");
  }

  if (exitCode) {
    if (report.lifecycle === "merged") process.exit(2);
    if (report.lifecycle === "closed") process.exit(3);
    if (!report.mergeable) process.exit(1);
  }
}

main().catch((error: unknown) => {
  if (error instanceof PreconditionError) {
    die(error.message);
  }
  if (process.env["DEBUG"]) {
    console.error(error);
  }
  die(error instanceof Error ? error.message : "unexpected error");
});
