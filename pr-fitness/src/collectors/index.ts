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
import { log } from "../util/log.js";

import { collectChecks } from "./checks.js";
import { collectComments } from "./comments.js";
import { collectCopilotRuleset } from "./copilot-ruleset.js";
import { collectGraphiteCheck } from "./graphite.js";
import { collectIssueEvents } from "./issue-events.js";
import { collectPrMetadata } from "./pr-metadata.js";
import { collectRequestedReviewers } from "./requested-reviewers.js";
import { collectReviewThreads } from "./review-threads.js";
import { collectReviews } from "./reviews.js";

export interface CollectedData {
  readonly pr: GitHubPullRequestView;
  readonly checks: readonly GitHubCheck[];
  readonly threads: GitHubPullRequestReviewThreadsResponse;
  readonly comments: readonly GitHubIssueComment[];
  readonly reviews: readonly GitHubPullRequestReview[];
  readonly graphite: GraphiteCheck;
  readonly lastCommitDate: string | null;
  readonly events: readonly GitHubIssueEvent[];
  readonly requestedReviewers: GitHubRequestedReviewers;
  readonly copilotConfig: CopilotRepoConfig;
}

/** Run all API calls in parallel and return collected data. */
export async function collect(
  repo: RepoSlug,
  pr: PullRequestNumber,
): Promise<CollectedData> {
  const ownerPart = owner(repo);
  const namePart = name(repo);

  log(`querying PR #${String(pr)} in ${repo}...`);

  const [
    prData,
    checks,
    threads,
    comments,
    reviews,
    graphiteResult,
    events,
    requestedReviewers,
    copilotConfig,
  ] = await Promise.all([
    collectPrMetadata(repo, pr),
    collectChecks(repo, pr),
    collectReviewThreads(ownerPart, namePart, pr),
    collectComments(repo, pr),
    collectReviews(repo, pr),
    collectGraphiteCheck(ownerPart, namePart, pr),
    collectIssueEvents(repo, pr),
    collectRequestedReviewers(repo, pr),
    collectCopilotRuleset(repo),
  ]);

  log("done");

  return {
    pr: prData,
    checks,
    threads,
    comments,
    reviews,
    graphite: graphiteResult.check,
    lastCommitDate: graphiteResult.lastCommitDate,
    events,
    requestedReviewers,
    copilotConfig,
  };
}
