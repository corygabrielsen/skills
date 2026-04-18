#!/usr/bin/env npx tsx
/**
 * Wrapper that invokes converge2 with the right --hook and fitness
 * command for pr-fitness. This is what SKILL.md tells agents to run.
 *
 * Usage:
 *   npx tsx converge-wrapper.ts <owner/repo> <pr> [converge2-opts...]
 *   npx tsx converge-wrapper.ts <pr> [converge2-opts...]
 *
 * When only a PR number is given, infers the repo from the current
 * directory's git remote via `gh repo view`.
 */

import { execFileSync, execSync } from "node:child_process";
import * as os from "node:os";
import * as path from "node:path";

const CONVERGE2 = path.join(
  os.homedir(),
  "code",
  "skills",
  "converge2",
  "target",
  "release",
  "converge2",
);

const FITNESS_CLI = path.join(
  os.homedir(),
  "code",
  "skills",
  "pr-fitness",
  "src",
  "cli.ts",
);

const HOOK_CLI = path.join(
  os.homedir(),
  "code",
  "skills",
  "pr-fitness",
  "src",
  "pr-progress-hook.ts",
);

function die(msg: string): never {
  process.stderr.write(`pr-converge: ${msg}\n`);
  process.exit(64);
}

function inferRepo(): string {
  try {
    return execFileSync(
      "gh",
      ["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
      { encoding: "utf8", timeout: 10_000 },
    ).trim();
  } catch {
    die("cannot infer repo — pass <owner/repo> explicitly");
  }
}

// Parse args: positional args are repo + PR, everything else passes
// through to converge2.
const args = process.argv.slice(2);
const convergeOpts: string[] = [];
const positional: string[] = [];

const FLAGS_WITH_VALUES = new Set(["-n", "--max-iter", "-s", "--session"]);
let i = 0;
while (i < args.length) {
  const arg = args[i]!;
  if (arg.startsWith("-")) {
    convergeOpts.push(arg);
    if (FLAGS_WITH_VALUES.has(arg) && i + 1 < args.length) {
      i++;
      convergeOpts.push(args[i]!);
    }
  } else {
    positional.push(arg);
  }
  i++;
}

let repo: string;
let pr: string;

if (positional.length === 2) {
  repo = positional[0]!;
  pr = positional[1]!;
} else if (positional.length === 1 && /^\d+$/.test(positional[0]!)) {
  repo = inferRepo();
  pr = positional[0]!;
} else {
  die("usage: pr-converge <owner/repo> <pr> [opts...]\n       pr-converge <pr> [opts...]");
}

const sessionId = `pr-${repo.replace(/[^A-Za-z0-9_-]/g, "-")}-${pr}`;
const verbose = convergeOpts.includes("-v") || convergeOpts.includes("--verbose");

const hookCmd = [
  "npx", "tsx", HOOK_CLI, repo, pr,
].join(" ");

const convergeArgs = [
  "-s", sessionId,
  ...convergeOpts,
  "--hook", hookCmd,
  "--",
  "npx", "tsx", FITNESS_CLI, "-q", "-c", repo, pr,
];

if (verbose) {
  process.stderr.write(`pr-converge: ${CONVERGE2} ${convergeArgs.join(" ")}\n`);
}

// Replace this process with converge2.
try {
  execSync(
    [CONVERGE2, ...convergeArgs].map(a => `'${a}'`).join(" "),
    { stdio: "inherit", env: { ...process.env, ...(verbose ? { VERBOSE: "1" } : {}) } },
  );
} catch (err: unknown) {
  // execSync throws on non-zero exit. Extract the exit code.
  if (err && typeof err === "object" && "status" in err) {
    process.exit((err as { status: number }).status);
  }
  process.exit(4);
}
