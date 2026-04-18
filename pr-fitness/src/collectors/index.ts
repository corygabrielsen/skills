import type { PullRequestNumber, RepoSlug } from "../types/branded.js";
import { name, owner } from "../types/branded.js";
import type {
  GitHubCheck,
  GitHubIssueComment,
  GitHubIssueEvent,
  GitHubPullRequestView,
  GitHubPullRequestReview,
  GitHubPullRequestReviewThreadsResponse,
  GitHubRequestedReviewers,
  RequiredCheckConfig,
} from "../types/index.js";
import type { CopilotRepoConfig } from "../types/copilot.js";
import type { GraphiteCheck } from "../types/output.js";
import { CollectorError } from "../util/collector-error.js";
import { log } from "../util/log.js";

import { collectChecks } from "./checks.js";
import { collectComments } from "./comments.js";
import { collectCopilotRuleset } from "./copilot-ruleset.js";
import type { GraphiteCollectorResult } from "./graphite.js";
import {
  collectGraphiteCheck,
  EMPTY_RESULT as EMPTY_GRAPHITE,
} from "./graphite.js";
import { collectIssueEvents } from "./issue-events.js";
import { collectPrMetadata } from "./pr-metadata.js";
import { collectRequestedReviewers } from "./requested-reviewers.js";
import {
  collectRequiredCheckConfig,
  resolveStackRoot,
} from "./required-checks.js";
import { collectReviewThreads, EMPTY_THREADS } from "./review-threads.js";
import { collectReviews } from "./reviews.js";

export interface CollectedData {
  readonly pr: GitHubPullRequestView;
  /** Branch the stack ultimately targets (e.g. master for mid-stack PRs). */
  readonly stackRoot: string;
  readonly checks: readonly GitHubCheck[];
  readonly requiredCheckConfig: readonly RequiredCheckConfig[];
  readonly threads: GitHubPullRequestReviewThreadsResponse;
  readonly comments: readonly GitHubIssueComment[];
  readonly reviews: readonly GitHubPullRequestReview[];
  readonly graphite: GraphiteCheck;
  readonly lastCommitDate: string | null;
  readonly events: readonly GitHubIssueEvent[];
  readonly requestedReviewers: GitHubRequestedReviewers;
  readonly copilotConfig: CopilotRepoConfig;
  readonly degraded: readonly CollectorError[];
}

// ---------------------------------------------------------------------------
// Non-fatal collector descriptor: name, thunk, and fallback value.
// ---------------------------------------------------------------------------

interface NonFatalCollector<T> {
  readonly name: string;
  readonly thunk: () => Promise<T>;
  readonly fallback: T;
}

/**
 * Settle a non-fatal collector: on `CollectorError`, return fallback
 * and append to `degraded`. Any other error is re-thrown (unexpected).
 */
async function settle<T>(
  c: NonFatalCollector<T>,
  degraded: CollectorError[],
): Promise<T> {
  try {
    return await c.thunk();
  } catch (err) {
    if (err instanceof CollectorError) {
      degraded.push(err);
      return c.fallback;
    }
    throw err;
  }
}

/** Run all API calls in parallel and return collected data. */
export async function collect(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<CollectedData> {
  const ownerPart = owner(repo);
  const namePart = name(repo);

  log(`querying PR #${String(pr)} in ${repo}...`);

  // Fatal: fail fast before launching non-fatal collectors.
  const prData = await collectPrMetadata(repo, pr);

  // Non-fatal: settled individually — failures degrade, not crash.
  const degraded: CollectorError[] = [];

  const checks: NonFatalCollector<readonly GitHubCheck[]> = {
    name: "checks",
    thunk: () => collectChecks(repo, pr),
    fallback: [],
  };
  // Stack root resolution runs inside the required-checks thunk so it
  // overlaps with the other collectors in Promise.all. The resolved
  // root is captured for CollectedData.
  let stackRoot = prData.baseRefName;
  const requiredCheckConfig: NonFatalCollector<readonly RequiredCheckConfig[]> =
    {
      name: "required-checks",
      thunk: async () => {
        stackRoot = await resolveStackRoot(repo, prData.baseRefName);
        return collectRequiredCheckConfig(repo, stackRoot);
      },
      fallback: [],
    };
  const threads: NonFatalCollector<GitHubPullRequestReviewThreadsResponse> = {
    name: "review-threads",
    thunk: () => collectReviewThreads(ownerPart, namePart, pr),
    fallback: EMPTY_THREADS,
  };
  const comments: NonFatalCollector<readonly GitHubIssueComment[]> = {
    name: "comments",
    thunk: () => collectComments(repo, pr),
    fallback: [],
  };
  const reviews: NonFatalCollector<readonly GitHubPullRequestReview[]> = {
    name: "reviews",
    thunk: () => collectReviews(repo, pr),
    fallback: [],
  };
  const graphiteResult: NonFatalCollector<GraphiteCollectorResult> = {
    name: "graphite",
    thunk: () => collectGraphiteCheck(ownerPart, namePart, pr),
    fallback: EMPTY_GRAPHITE,
  };
  const events: NonFatalCollector<readonly GitHubIssueEvent[]> = {
    name: "issue-events",
    thunk: () => collectIssueEvents(repo, pr),
    fallback: [],
  };
  const requestedReviewers: NonFatalCollector<GitHubRequestedReviewers> = {
    name: "requested-reviewers",
    thunk: () => collectRequestedReviewers(repo, pr),
    fallback: { users: [], teams: [] },
  };
  const copilotConfig: NonFatalCollector<CopilotRepoConfig> = {
    name: "copilot-ruleset",
    thunk: () => collectCopilotRuleset(repo),
    fallback: {
      enabled: false,
      reviewOnPush: false,
      reviewDraftPullRequests: false,
    },
  };

  const [
    checksVal,
    requiredCheckConfigVal,
    threadsVal,
    commentsVal,
    reviewsVal,
    graphiteVal,
    eventsVal,
    requestedReviewersVal,
    copilotConfigVal,
  ] = await Promise.all([
    settle(checks, degraded),
    settle(requiredCheckConfig, degraded),
    settle(threads, degraded),
    settle(comments, degraded),
    settle(reviews, degraded),
    settle(graphiteResult, degraded),
    settle(events, degraded),
    settle(requestedReviewers, degraded),
    settle(copilotConfig, degraded),
  ]);

  log("done");

  return {
    pr: prData,
    stackRoot,
    checks: checksVal,
    requiredCheckConfig: requiredCheckConfigVal,
    threads: threadsVal,
    comments: commentsVal,
    reviews: reviewsVal,
    graphite: graphiteVal.check,
    lastCommitDate: graphiteVal.lastCommitDate,
    events: eventsVal,
    requestedReviewers: requestedReviewersVal,
    copilotConfig: copilotConfigVal,
    degraded,
  };
}
