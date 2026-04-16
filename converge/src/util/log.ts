/**
 * Structured logging for /converge.
 *
 * Two sinks, both stderr:
 * - `log()` always prints (iter trace, halt line, warnings).
 * - `verbose()` prints only when `--verbose` was set via `setVerbose(true)`.
 *
 * stdout is reserved for machine-readable halt output; nothing here touches it.
 */

let verboseEnabled = false;

export function setVerbose(v: boolean): void {
  verboseEnabled = v;
}

export function log(message: string): void {
  process.stderr.write(`${message}\n`);
}

export function verbose(message: string): void {
  if (verboseEnabled) {
    process.stderr.write(`${message}\n`);
  }
}
