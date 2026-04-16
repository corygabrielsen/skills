import { GRAPHITE_MERGEABILITY_CHECK } from "../constants.js";
import type {
  AdvisorySummary,
  CheckBucketSummary,
  CiSummary,
  FailedCheck,
  GitHubCheck,
} from "../types/index.js";

interface Buckets {
  pass: number;
  pendingNames: string[];
  failedDetails: FailedCheck[];
}

function bucketSummary(b: Buckets): CheckBucketSummary {
  return {
    pass: b.pass,
    fail: b.failedDetails.length,
    pending: b.pendingNames.length,
    total: b.pass + b.failedDetails.length + b.pendingNames.length,
    failed: b.failedDetails.map((d) => d.name),
    pending_names: b.pendingNames,
    failed_details: b.failedDetails,
  };
}

/**
 * Summarize CI, splitting checks into **required** (gate merge) and
 * **advisory** (reported but don't gate). `requiredNames` is the list
 * from `gh pr checks --required`; an empty list means nothing is
 * required (a legitimate config — all checks become advisory).
 *
 * Graphite's mergeability check is treated as stack ordering, not CI,
 * and is excluded from both groups.
 */
export function computeCi(
  checks: readonly GitHubCheck[],
  requiredNames: readonly string[],
): CiSummary {
  const requiredSet = new Set(requiredNames);
  const required: Buckets = { pass: 0, pendingNames: [], failedDetails: [] };
  const advisory: Buckets = { pass: 0, pendingNames: [], failedDetails: [] };
  let completedAt: string | null = null;

  for (const c of checks) {
    if (c.name === GRAPHITE_MERGEABILITY_CHECK) continue;

    if (c.completedAt && (!completedAt || c.completedAt > completedAt)) {
      completedAt = c.completedAt;
    }

    const bucket = requiredSet.has(c.name) ? required : advisory;

    switch (c.state) {
      case "SUCCESS":
      case "SKIPPED":
      case "NEUTRAL":
        bucket.pass++;
        break;
      case "FAILURE":
        bucket.failedDetails.push({
          name: c.name,
          description: c.description,
          link: c.link,
        });
        break;
      case "IN_PROGRESS":
      case "QUEUED":
        bucket.pendingNames.push(c.name);
        break;
    }
  }

  const advisorySummary: AdvisorySummary = bucketSummary(advisory);
  return {
    ...bucketSummary(required),
    completed_at: completedAt,
    advisory: advisorySummary,
  };
}
