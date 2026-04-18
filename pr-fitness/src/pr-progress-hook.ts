#!/usr/bin/env npx tsx
/**
 * Hook coprocess for converge2. Reads JSONL events from stdin,
 * posts PR comments via the existing pr-progress rendering logic.
 *
 * Usage (as converge2 --hook argument):
 *   npx tsx pr-progress-hook.ts <owner/repo> <pr-number>
 *
 * Expects two positional arguments: the repo slug and PR number.
 * These are used to construct the PrProgressTarget for gh.
 */

import * as readline from "node:readline";

import {
  reportIteration,
  reportHalt,
  setPrProgressVerbose,
  type FitnessReportView,
  type ActionView,
  type HaltReportView,
  type PrProgressTarget,
} from "./pr-progress.js";

const repo = process.argv[2];
const pr = process.argv[3];

if (!repo || !pr) {
  process.stderr.write("usage: pr-progress-hook <owner/repo> <pr>\n");
  process.exit(1);
}

// Inherit verbose from VERBOSE env var (set by the wrapper).
if (process.env["VERBOSE"] === "1") {
  setPrProgressVerbose(true);
}

const target: PrProgressTarget = { repo, pr };
const rl = readline.createInterface({ input: process.stdin });

for await (const line of rl) {
  if (line.trim().length === 0) continue;

  let event: {
    event: string;
    iter?: number;
    report?: FitnessReportView;
    action?: ActionView;
    halt?: HaltReportView;
    last_report?: FitnessReportView;
  };

  try {
    event = JSON.parse(line);
  } catch {
    continue; // Skip malformed lines.
  }

  switch (event.event) {
    case "iteration":
      if (event.iter !== undefined && event.report && event.action) {
        await reportIteration(target, event.iter, event.report, event.action);
      }
      break;
    case "halt":
      if (event.halt) {
        await reportHalt(target, event.halt, event.last_report);
      }
      break;
  }
}
