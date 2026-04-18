/**
 * Main loop: observe (fitness) → decide (pickAction) → act → halt.
 *
 * Invariants:
 *   - Exactly one SessionHandle is held for the lifetime of `converge()`.
 *     Released in `finally`, including on cancellation.
 *   - `history` grows by one IterLog per observation (before action dispatch).
 *     Iteration count restarts at `history.length + 1` to support resume
 *     after `agent_needed` halts.
 *   - First terminal condition wins (target → terminal → stalled → action
 *     dispatch). This mirrors the v3 spec precedence.
 *   - AbortSignal from the caller composes with per-iteration children;
 *     `AbortError` from anywhere inside is funneled to a `cancelled` halt.
 *
 * The loop itself has no timers — `wait` actions sleep on the composed
 * signal, and the iteration cap bounds total work.
 */

import { executeFull } from "./execute.js";
import { invokeFitness } from "./fitness.js";
import {
  finalizeHalt,
  printHaltLine,
  printResumeHint,
  printTraceLine,
  writeHalt,
  writeInProgress,
} from "./halt.js";
import { appendHistory, openSession } from "./session.js";
import type {
  Action,
  FitnessReport,
  HaltBody,
  HaltReport,
  IterLog,
} from "./types/index.js";
import { Score } from "./types/index.js";
import {
  ExecuteError,
  FitnessUnavailableError,
  LockHeldError,
  PreconditionError,
} from "./util/errors.js";
import { verbose } from "./util/log.js";
import { sleep } from "./util/sleep.js";

export interface ConvergeOpts {
  readonly fitnessArgv: readonly string[];
  readonly maxIterations: number;
  readonly sessionId: string;
  /**
   * Skill-form invocation a caller can re-run after a resumable halt
   * (agent_needed). Written to exit.json's `resume_cmd` header field.
   * Example: `["/converge", "/my-fitness", "arg1", "arg2"]`.
   */
  readonly resumeCmd: readonly string[];
  /** Called after each non-agent, non-human iteration for progress reporting. */
  readonly onIteration?: (iter: number, report: FitnessReport, action: Action) => Promise<void>;
  /** Called after the loop halts, for final-state reporting. */
  readonly onHalt?: (halt: HaltReport, lastReport: FitnessReport | undefined) => Promise<void>;
  readonly signal: AbortSignal;
}

// ---------------------------------------------------------------------------

function cancelledBody(
  iter: number,
  score: Score,
  history: readonly IterLog[],
): Extract<HaltBody, { status: "cancelled" }> {
  return {
    status: "cancelled",
    iterations: iter,
    final_score: score,
    history: [...history],
  };
}

function pickAction(actions: readonly Action[]): Action | null {
  return actions.find((a) => a.target_effect !== "neutral") ?? null;
}

function targetReached(report: FitnessReport): boolean {
  return (report.score as number) >= (report.target as number);
}

function summary(action: Action): IterLog["action_summary"] {
  return { kind: action.kind, automation: action.automation };
}

function isAbortError(err: unknown): boolean {
  return err instanceof Error && err.name === "AbortError";
}

/**
 * Recursively sort object keys so `JSON.stringify` produces a stable string
 * regardless of source key order. Arrays preserve their order. Only plain
 * JSON-shaped values are expected (the fitness report is JSON on the wire).
 */
function stableStringify(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(stableStringify).join(",")}]`;
  }
  const entries = Object.entries(value as Record<string, unknown>).sort(
    ([a], [b]) => (a < b ? -1 : a > b ? 1 : 0),
  );
  return `{${entries
    .map(([k, v]) => `${JSON.stringify(k)}:${stableStringify(v)}`)
    .join(",")}}`;
}

/**
 * Iteration key: identity of a logical state. Two observations with the
 * same key represent the same iteration — they don't trigger a new
 * iteration, history entry, or progress callback.
 *
 * Deliberately excludes `score` from the key so tier progression under
 * the same action (bronze → silver on the same wait_for_ci) doesn't
 * churn. Score changes show up indirectly when they correlate with
 * action, blocker-set, or activity changes.
 */
function iterKey(action: Action, report: FitnessReport): string {
  const blockers = [...(report.blockers ?? [])].sort().join("|");
  const activity = stableStringify(report.activity_state ?? {});
  const typeDigest = stableStringify(action.type ?? null);
  return `${action.kind}\0${typeDigest}\0${blockers}\0${activity}`;
}

/**
 * Max polls per logical iteration. A session that never advances the
 * key over 20 consecutive polls is treated as runaway and halted with
 * `timeout`. Generous enough to cover real external-service poll cycles
 * at 60s cadence (~20 minutes).
 */
const MAX_POLLS_PER_ITERATION = 20;

/**
 * Minimum delay before re-observing after a `full` action executed.
 * Guards against tight-looping when external state hasn't propagated
 * (e.g. an action just fired but the next observation still sees the
 * old activity). Set conservatively — state usually propagates within
 * seconds; this is a floor, not a budget.
 */
const POST_FULL_REOBSERVE_MS = 15_000;

// ---------------------------------------------------------------------------

export async function converge(opts: ConvergeOpts): Promise<HaltReport> {
  if (opts.maxIterations <= 0 || !Number.isInteger(opts.maxIterations)) {
    throw new PreconditionError(
      `invalid maxIterations: ${String(opts.maxIterations)}`,
    );
  }

  // Lock acquisition is outside the try/finally because there is no
  // handle to release until we have one.
  let handle;
  try {
    handle = await openSession(opts.sessionId);
  } catch (err) {
    if (err instanceof LockHeldError) throw err;
    throw err;
  }

  const sessionDir = handle.dir;
  const history = handle.history;

  // Iteration counter resumes after any prior halts (e.g. agent_needed).
  const startIter = history.length + 1;
  const zeroScore = Score(0);
  let lastScore = history[history.length - 1]?.score ?? zeroScore;
  // Most recent successful observation — passed to onHalt so the callback
  // can render branded score display. Undefined until first observation.
  let lastReport: FitnessReport | undefined;

  // Per-iteration progress callbacks chain onto this tail so the loop's
  // critical path never blocks on I/O. The chain (vs parallel
  // fire-and-forget) preserves timeline order across iterations and
  // caps concurrency at 1. `finalize` awaits the tail before
  // the halt callback so iteration reports land first.
  let progressTail: Promise<void> = Promise.resolve();

  // Stub exit.json so a consumer never reads a stale prior run's halt as
  // belonging to this invocation. Overwritten by finalize() on halt.
  await writeInProgress(sessionDir, opts.sessionId, opts.resumeCmd);

  const finalize = async (body: HaltBody): Promise<HaltReport> => {
    const report = finalizeHalt(body, opts.sessionId, opts.resumeCmd);
    await writeHalt(sessionDir, report);
    printHaltLine(report);
    printResumeHint(report);
    await progressTail;
    if (opts.onHalt) {
      await opts.onHalt(report, lastReport).catch(() => undefined);
    }
    return report;
  };

  // Logical iteration counter. Only advances on state transition (new
  // iterKey vs prior). Polling while in the same state does not count.
  // `currentIterKey` is null at session start so the first observation
  // always advances — even on resume, a fresh invocation gets at least
  // one new logical iteration.
  let iter = startIter - 1;
  let currentIterKey: string | null = null;
  // Safety ceiling on total polls regardless of state transitions —
  // prevents runaway sessions when a wait condition never resolves.
  const maxPolls = opts.maxIterations * MAX_POLLS_PER_ITERATION;
  let pollCount = 0;

  try {
    while (pollCount < maxPolls) {
      if (opts.signal.aborted) {
        return await finalize(cancelledBody(iter, lastScore, history));
      }
      pollCount++;

      verbose(`converge: poll ${pollCount} (iter ${iter})`);

      // Observe.
      let report: FitnessReport;
      try {
        report = await invokeFitness(opts.fitnessArgv, opts.signal);
      } catch (err) {
        if (isAbortError(err)) {
          return await finalize(cancelledBody(iter, lastScore, history));
        }
        if (err instanceof FitnessUnavailableError) {
          return await finalize({
            status: "fitness_unavailable",
            iterations: iter,
            cause: {
              source: "fitness",
              message: err.message,
              stderr: err.lastStderr,
            },
            history: [...history],
          });
        }
        if (err instanceof PreconditionError) {
          return await finalize({
            status: "error",
            iterations: iter,
            final_score: lastScore,
            cause: {
              source: "parse",
              message: err.message,
            },
            history: [...history],
          });
        }
        return await finalize({
          status: "error",
          iterations: iter,
          final_score: lastScore,
          cause: {
            source: "fitness",
            message: err instanceof Error ? err.message : String(err),
          },
          history: [...history],
        });
      }

      lastScore = report.score;
      lastReport = report;

      const action = pickAction(report.actions);

      if (targetReached(report)) {
        const structural = report.blocker_split?.structural ?? [];
        return await finalize({
          status: "success",
          iterations: iter,
          final_score: report.score,
          ...(structural.length > 0 ? { structural_blockers: structural } : {}),
          history: [...history],
        });
      }
      if (report.terminal !== undefined) {
        return await finalize({
          status: "terminal",
          iterations: iter,
          final_score: report.score,
          terminal: report.terminal,
          history: [...history],
        });
      }
      if (action === null) {
        return await finalize({
          status: "stalled",
          iterations: iter,
          final_score: report.score,
          history: [...history],
        });
      }

      // Compute iteration key and decide whether this observation
      // represents a new logical iteration.
      const newKey = iterKey(action, report);
      const isNewIteration = newKey !== currentIterKey;

      if (isNewIteration) {
        currentIterKey = newKey;
        iter++;

        if (iter >= startIter + opts.maxIterations) {
          return await finalize({
            status: "timeout",
            iterations: iter - 1,
            final_score: report.score,
            history: [...history],
          });
        }

        await appendHistory(handle, {
          iter,
          score: report.score,
          action_summary: summary(action),
        });

        printTraceLine(iter, report.score, action);
        // For agent/human actions the halt fires immediately with the
        // same description, so skip the iteration callback to avoid
        // near-identical duplicate reports.
        if (opts.onIteration && action.automation !== "agent" && action.automation !== "human") {
          const cb = opts.onIteration;
          const iterSnap = iter;
          const reportSnap = report;
          const actionSnap = action;
          progressTail = progressTail.then(() => cb(iterSnap, reportSnap, actionSnap).catch(() => undefined));
        }
      }

      // `full` executes only on a new iteration — otherwise we'd
      // repeat work the prior loop turn already did.
      switch (action.automation) {
        case "full": {
          if (!isNewIteration) {
            try {
              await sleep(POST_FULL_REOBSERVE_MS, opts.signal);
            } catch (err) {
              if (isAbortError(err)) {
                return await finalize(
                  cancelledBody(iter, report.score, history),
                );
              }
              throw err;
            }
            break;
          }
          try {
            await executeFull(action, sessionDir, iter, opts.signal);
          } catch (err) {
            if (isAbortError(err)) {
              return await finalize(cancelledBody(iter, report.score, history));
            }
            if (err instanceof ExecuteError) {
              return await finalize({
                status: "error",
                iterations: iter,
                final_score: report.score,
                cause: {
                  source: "execute",
                  message: err.message,
                  stderr: err.stderr,
                  action_kind: err.actionKind,
                },
                history: [...history],
              });
            }
            return await finalize({
              status: "error",
              iterations: iter,
              final_score: report.score,
              cause: {
                source: "execute",
                message: err instanceof Error ? err.message : String(err),
                action_kind: action.kind,
              },
              history: [...history],
            });
          }
          break;
        }
        case "agent": {
          return await finalize({
            status: "agent_needed",
            iterations: iter,
            final_score: report.score,
            action,
            history: [...history],
          });
        }
        case "wait": {
          const ms = (action.next_poll_seconds as number) * 1000;
          try {
            await sleep(ms, opts.signal);
          } catch (err) {
            if (isAbortError(err)) {
              return await finalize(cancelledBody(iter, report.score, history));
            }
            throw err;
          }
          break;
        }
        case "human": {
          return await finalize({
            status: "hil",
            iterations: iter,
            final_score: report.score,
            action,
            history: [...history],
          });
        }
      }
    }

    // Poll cap hit without hitting the logical iteration cap —
    // typically means a wait condition never resolved.
    return await finalize({
      status: "timeout",
      iterations: iter,
      final_score: lastScore,
      history: [...history],
    });
  } finally {
    await handle.release().catch(() => undefined);
  }
}
