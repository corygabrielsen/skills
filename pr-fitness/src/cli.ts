#!/usr/bin/env node

import { prFitness } from "./pr-fitness.js";
import { GhError, PreconditionError } from "./util/errors.js";
import { setQuiet } from "./util/log.js";
import { VERSION } from "./version.js";

function usage(): never {
  process.stdout.write(`Usage: pr-fitness [options] <owner/repo> <pr_number>

Live PR merge readiness assessment. Queries all state fresh.

Options:
  -h, --help       Show this help
  -v, --version    Show version
  -q, --quiet      Suppress stderr progress
  -c, --compact    Compact JSON (single line)
  -s, --summary    Print one-line summary instead of JSON
  -e, --exit-code  Exit code reflects state (for scripts)
                     0 = open + mergeable
                     1 = open + blocked
                     2 = already merged
                     3 = closed (not merged)

Examples:
  pr-fitness example/widgets 1563
  pr-fitness -c example/widgets 1563
  pr-fitness -q example/widgets 1563 | jq '.blockers'

Output fields:
  .lifecycle       open, merged, or closed
  .mergeable       true if all hard blockers clear
  .blockers[]      list of blocking issues (empty when mergeable)
  .ci              check pass/fail/pending counts and names
  .reviews         approval state, threads, bot comments
  .state           draft, labels, title, timestamps, assignees
  .actions[]       ordered plan to increase fitness
  .duration_ms     time to generate report
`);
  process.exit(0);
}

function die(message: string): never {
  process.stderr.write(`pr-fitness: ${message}\n`);
  process.exit(1);
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const positional: string[] = [];
  let compact = false;
  let summaryOnly = false;
  let exitCode = false;

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
    } else if (arg.startsWith("-")) {
      die(`unknown option: ${arg}`);
    } else {
      positional.push(arg);
    }
  }

  const repo = positional[0];
  const prStr = positional[1];

  if (!repo) die("missing argument: <owner/repo>");
  if (!prStr) die("missing argument: <pr_number>");
  if (!repo.includes("/"))
    die(`invalid repo format: expected owner/repo, got ${repo}`);

  const pr = Number(prStr);
  if (!Number.isInteger(pr) || pr <= 0) die(`invalid PR number: ${prStr}`);

  const report = await prFitness(repo, pr);

  if (summaryOnly) {
    process.stdout.write(report.summary + "\n");
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
  if (error instanceof GhError) {
    die(error.message);
  }
  if (process.env["DEBUG"]) {
    console.error(error);
  }
  die(error instanceof Error ? error.message : "unexpected error");
});
