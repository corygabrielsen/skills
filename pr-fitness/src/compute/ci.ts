import { GRAPHITE_MERGEABILITY_CHECK } from "../constants.js";
import type {
  AdvisorySummary,
  CheckBucketSummary,
  CiSummary,
  FailedCheck,
  GitHubCheck,
  RequiredCheckConfig,
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
 * Join configured required checks with observed check-runs.
 *
 * `requiredConfig` is the authoritative set of check names that must
 * pass before merge — sourced from branch rules (rulesets + legacy
 * protection). Each configured check resolves against `checks`:
 *
 *   - Present + success/skipped/neutral → pass
 *   - Present + failure                → fail
 *   - Present + in_progress/queued     → pending
 *   - Absent                           → missing (skipped job, timing race)
 *
 * Observed checks not in `requiredConfig` route to advisory.
 *
 * Graphite's mergeability check is excluded from CI counts entirely —
 * it's a stack-ordering gate handled by the graphite subsystem.
 */
export function computeCi(
  checks: readonly GitHubCheck[],
  requiredConfig: readonly RequiredCheckConfig[],
): CiSummary {
  // Filter Graphite from the configured required set — it's handled
  // separately by the graphite collector and blocker.
  const ciRequired = requiredConfig.filter(
    (c) => c.context !== GRAPHITE_MERGEABILITY_CHECK,
  );
  const requiredContexts = new Set(ciRequired.map((c) => c.context));

  // Index observed checks by name for O(1) join.
  const observed = new Map<string, GitHubCheck>();
  for (const c of checks) {
    observed.set(c.name, c);
  }

  // Resolve each configured requirement against observations.
  const required: Buckets = { pass: 0, pendingNames: [], failedDetails: [] };
  const missingNames: string[] = [];
  let completedAt: string | null = null;

  for (const config of ciRequired) {
    const obs = observed.get(config.context);
    if (!obs) {
      missingNames.push(config.context);
      continue;
    }
    if (obs.completedAt && (!completedAt || obs.completedAt > completedAt)) {
      completedAt = obs.completedAt;
    }
    switch (obs.state) {
      case "SUCCESS":
      case "SKIPPED":
      case "NEUTRAL":
        required.pass++;
        break;
      case "FAILURE":
        required.failedDetails.push({
          name: obs.name,
          description: obs.description,
          link: obs.link,
        });
        break;
      case "IN_PROGRESS":
      case "QUEUED":
        required.pendingNames.push(obs.name);
        break;
    }
  }

  // Advisory: observed but not required (excluding Graphite).
  const advisory: Buckets = { pass: 0, pendingNames: [], failedDetails: [] };
  for (const c of checks) {
    if (requiredContexts.has(c.name)) continue;
    if (c.name === GRAPHITE_MERGEABILITY_CHECK) continue;
    if (c.completedAt && (!completedAt || c.completedAt > completedAt)) {
      completedAt = c.completedAt;
    }
    switch (c.state) {
      case "SUCCESS":
      case "SKIPPED":
      case "NEUTRAL":
        advisory.pass++;
        break;
      case "FAILURE":
        advisory.failedDetails.push({
          name: c.name,
          description: c.description,
          link: c.link,
        });
        break;
      case "IN_PROGRESS":
      case "QUEUED":
        advisory.pendingNames.push(c.name);
        break;
    }
  }

  const advisorySummary: AdvisorySummary = bucketSummary(advisory);
  return {
    ...bucketSummary(required),
    missing: missingNames.length,
    missing_names: missingNames,
    completed_at: completedAt,
    advisory: advisorySummary,
  };
}
