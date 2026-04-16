/**
 * Stable names GitHub assigns to external checks pr-fitness treats
 * specially. Collectors and compute both need these to be the same
 * string or the "special" handling silently stops applying.
 */

/**
 * Graphite's stack-ordering check. pr-fitness reads its state via the
 * graphite collector and excludes it from CI counts — it's a pre-merge
 * gate for stacked PRs, not a CI signal.
 */
export const GRAPHITE_MERGEABILITY_CHECK = "Graphite / mergeability_check";
