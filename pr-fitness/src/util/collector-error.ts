/**
 * Collector-level error wrapper.
 *
 * I₂ invariant: every collector either returns its domain type or
 * throws `CollectorError` with the classified `GhError` attached.
 * No collector propagates raw `GhError` — the coproduct is eliminated
 * at the collector boundary.
 */

import type { GhError, GhErrorMatch } from "./gh.js";

export class CollectorError extends Error {
  constructor(
    readonly collector: string,
    readonly ghError: GhError,
  ) {
    super(`collector ${collector}: ${ghError.kind}`);
    this.name = "CollectorError";
  }
}

/**
 * Base `GhErrorMatch` that throws `CollectorError` for every variant.
 *
 * Collectors spread this as the default and override specific variants
 * with domain-appropriate fallbacks:
 *
 *     match(result.error, { ...ghErrorThrow("checks"), empty: () => [] })
 */
export function ghErrorThrow(collector: string): GhErrorMatch<never> {
  const raise = (e: GhError): never => {
    throw new CollectorError(collector, e);
  };
  return {
    empty: raise,
    not_found: raise,
    auth: raise,
    rate_limit: raise,
    network: raise,
    unknown: raise,
  };
}
