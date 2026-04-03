import type { Lifecycle } from "../types/output.js";

/** Human-readable one-liner for the report. */
export function summarize(
  lifecycle: Lifecycle,
  blockers: readonly string[],
  mergedAt: string | null,
): string {
  if (lifecycle === "merged") {
    return `Merged${mergedAt ? ` ${mergedAt}` : ""}`;
  }
  if (lifecycle === "closed") return "Closed (not merged)";
  if (blockers.length === 0) return "Ready to merge";
  return `Blocked: ${blockers.join(", ")}`;
}
