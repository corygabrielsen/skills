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
} from "../types/index.js";
import type { CopilotRepoConfig } from "../types/copilot.js";
import type { GraphiteCheck } from "../types/output.js";
import { CollectorError } from "../util/collector-error.js";
import { log } from "../util/log.js";

import { collectChecks } from "./checks.js";
import { collectComments } from "./comments.js";
import { collectCopilotRuleset } from "./copilot-ruleset.js";
import type { GraphiteCollectorResult } from "./graphite.js";
import { collectGraphiteCheck } from "./graphite.js";
import { collectIssueEvents } from "./issue-events.js";
import { collectPrMetadata } from "./pr-metadata.js";
import { collectRequestedReviewers } from "./requested-reviewers.js";
import { collectRequiredCheckNames } from "./required-checks.js";
import { collectReviewThreads } from "./review-threads.js";
import { collectReviews } from "./reviews.js";

export interface CollectedData {
  readonly pr: GitHubPullRequestView;
  readonly checks: readonly GitHubCheck[];
  readonly requiredCheckNames: readonly string[];
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

/** Empty review-threads response — mirrors EMPTY_THREADS in review-threads.ts. */
const EMPTY_THREADS: GitHubPullRequestReviewThreadsResponse = {
  data: {
    repository: {
      pullRequest: {
        reviewThreads: { nodes: [] },
        reviewRequests: { nodes: [] },
      },
    },
  },
};

/** Empty Graphite result — mirrors EMPTY_RESULT in graphite.ts. */
const EMPTY_GRAPHITE: GraphiteCollectorResult = {
  check: { status: "none", title: null, summary: null },
  lastCommitDate: null,
};

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
  const requiredCheckNames: NonFatalCollector<readonly string[]> = {
    name: "required-checks",
    thunk: () => collectRequiredCheckNames(repo, pr),
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
    requiredCheckNamesVal,
    threadsVal,
    commentsVal,
    reviewsVal,
    graphiteVal,
    eventsVal,
    requestedReviewersVal,
    copilotConfigVal,
  ] = await Promise.all([
    settle(checks, degraded),
    settle(requiredCheckNames, degraded),
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
    checks: checksVal,
    requiredCheckNames: requiredCheckNamesVal,
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
