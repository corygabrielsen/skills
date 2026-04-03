import type {
  GhCheck,
  GhIssueComment,
  GhPrView,
  GhReview,
  GhReviewThreadsResponse,
} from "../types/index.js";
import type { GraphiteCheck } from "../types/output.js";
import { log } from "../util/log.js";

import { collectChecks } from "./checks.js";
import { collectComments } from "./comments.js";
import { collectGraphiteCheck } from "./graphite.js";
import { collectPrMetadata } from "./pr-metadata.js";
import { collectReviewThreads } from "./review-threads.js";
import { collectReviews } from "./reviews.js";

export interface CollectedData {
  readonly pr: GhPrView;
  readonly checks: readonly GhCheck[];
  readonly threads: GhReviewThreadsResponse;
  readonly comments: readonly GhIssueComment[];
  readonly reviews: readonly GhReview[];
  readonly graphite: GraphiteCheck;
  readonly lastCommitDate: string | null;
}

/** Run all API calls in parallel and return collected data. */
export async function collect(
  repo: string,
  pr: number,
): Promise<CollectedData> {
  const [owner, name] = repo.split("/");
  if (!owner || !name) throw new Error(`invalid repo: ${repo}`);

  log(`querying PR #${String(pr)} in ${repo}...`);

  const [prData, checks, threads, comments, reviews, graphiteResult] =
    await Promise.all([
      collectPrMetadata(repo, pr),
      collectChecks(repo, pr),
      collectReviewThreads(owner, name, pr),
      collectComments(repo, pr),
      collectReviews(repo, pr),
      collectGraphiteCheck(owner, name, pr),
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
  };
}
