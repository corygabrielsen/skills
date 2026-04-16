import type { CiSummary, FailedCheck, GitHubCheck } from "../types/index.js";

const GRAPHITE_CHECK = "Graphite / mergeability_check";

export function computeCi(checks: readonly GitHubCheck[]): CiSummary {
  let pass = 0;
  let completedAt: string | null = null;
  const failedDetails: FailedCheck[] = [];
  const pendingNames: string[] = [];

  for (const c of checks) {
    // Graphite's mergeability check is a stack-ordering guard, not CI.
    if (c.name === GRAPHITE_CHECK) continue;

    // Track the most recent completion time across all checks.
    if (c.completedAt && (!completedAt || c.completedAt > completedAt)) {
      completedAt = c.completedAt;
    }

    switch (c.state) {
      case "SUCCESS":
      case "SKIPPED":
      case "NEUTRAL":
        pass++;
        break;
      case "FAILURE":
        failedDetails.push({
          name: c.name,
          description: c.description,
          link: c.link,
        });
        break;
      case "IN_PROGRESS":
      case "QUEUED":
        pendingNames.push(c.name);
        break;
    }
  }

  return {
    pass,
    fail: failedDetails.length,
    pending: pendingNames.length,
    total: pass + failedDetails.length + pendingNames.length,
    failed: failedDetails.map((d) => d.name),
    pending_names: pendingNames,
    failed_details: failedDetails,
    completed_at: completedAt,
  };
}
