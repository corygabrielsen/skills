/**
 * Main loop: observe (fitness) → decide (pickAction) → act → halt.
 *
 * Invariants:
 *   - Exactly one SessionHandle is held for the lifetime of `converge()`.
 *     Released in `finally`, including on cancellation.
 *   - `history` grows by one IterLog per observation (before action dispatch).
 *     Iteration count restarts at `history.length + 1` to support resume
 *     after `llm_needed` halts.
 *   - First terminal condition wins (target → pr_terminal → stalled → action
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
  readonly fitness: string;
  readonly args: readonly string[];
  readonly maxIterations: number;
  readonly sessionId: string;
  /**
   * Skill-form invocation a caller can re-run after a resumable halt
   * (llm_needed). Written to exit.json's `resume_cmd` header field.
   * Example: `["/converge", "/pr-fitness", "example-org/repo", "566"]`.
   */
  readonly resumeCmd: readonly string[];
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

  // Iteration counter resumes after any prior halts (e.g. llm_needed).
  const startIter = history.length + 1;
  const zeroScore = Score(0);
  let lastScore = history[history.length - 1]?.score ?? zeroScore;

  // Stub exit.json so a consumer never reads a stale prior run's halt as
  // belonging to this invocation. Overwritten by finalize() on halt.
  await writeInProgress(sessionDir, opts.sessionId, opts.resumeCmd);

  const finalize = async (body: HaltBody): Promise<HaltReport> => {
    const report = finalizeHalt(body, opts.sessionId, opts.resumeCmd);
    await writeHalt(sessionDir, report);
    printHaltLine(report);
    printResumeHint(report);
    return report;
  };

  try {
    for (let iter = startIter; iter < startIter + opts.maxIterations; iter++) {
      if (opts.signal.aborted) {
        return await finalize(cancelledBody(iter - 1, lastScore, history));
      }

      verbose(`converge: iter ${iter}`);

      // Observe.
      let report: FitnessReport;
      try {
        report = await invokeFitness(opts.fitness, opts.args, opts.signal);
      } catch (err) {
        if (isAbortError(err)) {
          return await finalize(cancelledBody(iter - 1, lastScore, history));
        }
        if (err instanceof FitnessUnavailableError) {
          return await finalize({
            status: "fitness_unavailable",
            iterations: iter - 1,
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
            iterations: iter - 1,
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
          iterations: iter - 1,
          final_score: lastScore,
          cause: {
            source: "fitness",
            message: err instanceof Error ? err.message : String(err),
          },
          history: [...history],
        });
      }

      lastScore = report.score;

      // Decide.
      const action = pickAction(report.actions);

      // Record this iteration. Use the chosen action for the summary if
      // any; otherwise a synthetic marker so history shows the observation.
      const summaryEntry: IterLog["action_summary"] =
        action !== null
          ? summary(action)
          : { kind: "none", automation: "human" };
      await appendHistory(handle, {
        iter,
        score: report.score,
        action_summary: summaryEntry,
      });

      // Halt checks in precedence order.
      if (targetReached(report)) {
        return await finalize({
          status: "success",
          iterations: iter,
          final_score: report.score,
          history: [...history],
        });
      }
      if (report.terminal !== undefined) {
        return await finalize({
          status: "pr_terminal",
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

      printTraceLine(iter, report.score, action);

      // Act.
      switch (action.automation) {
        case "full": {
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
        case "llm": {
          return await finalize({
            status: "llm_needed",
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

    // Iteration cap exhausted.
    return await finalize({
      status: "timeout",
      iterations: startIter + opts.maxIterations - 1,
      final_score: lastScore,
      history: [...history],
    });
  } finally {
    await handle.release().catch(() => undefined);
  }
}
