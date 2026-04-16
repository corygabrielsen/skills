/**
 * Execute a FullAction subprocess.
 *
 * Semantics:
 *   - `spawn(execute[0], execute[1..])` with a detached process group so
 *     timeouts/aborts can reap the whole tree, not just the direct child.
 *   - Timeout = `action.timeout_seconds ?? 60` seconds. On expiry: SIGTERM
 *     the group, wait 5s grace, then SIGKILL.
 *   - Aborts (from the signal) do the same kill-sequence.
 *   - stdout/stderr both captured to `{sessionDir}/iter-{iter}.execute.log`.
 *     Per-stream cap 1 MiB; overflow truncated with a marker.
 *   - Exit 0 resolves; anything else (non-zero / timeout / abort) rejects
 *     with `ExecuteError` carrying the action kind and captured stderr.
 */

import { spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { createWriteStream } from "node:fs";
import * as path from "node:path";

import type { FullAction } from "./types/index.js";
import { PositiveSeconds } from "./types/index.js";
import { ExecuteError } from "./util/errors.js";
import { verbose } from "./util/log.js";

const STREAM_CAP_BYTES = 1024 * 1024;
const KILL_GRACE_MS = 5_000;
const TRUNCATION_MARKER = "\n[... truncated: stream exceeded 1 MiB ...]\n";

interface CappedBuffer {
  data: string;
  truncated: boolean;
  bytes: number;
}

function appendCapped(buf: CappedBuffer, chunk: string): void {
  if (buf.truncated) return;
  const room = STREAM_CAP_BYTES - buf.bytes;
  if (chunk.length <= room) {
    buf.data += chunk;
    buf.bytes += chunk.length;
    return;
  }
  buf.data += chunk.slice(0, room) + TRUNCATION_MARKER;
  buf.bytes = STREAM_CAP_BYTES;
  buf.truncated = true;
}

/**
 * Send `signal` to the child's whole process group when possible. Falls back
 * to killing the direct child. Swallows ESRCH (already gone).
 */
function killGroup(child: ChildProcess, sig: NodeJS.Signals): void {
  const pid = child.pid;
  if (pid === undefined) return;
  try {
    // Negative pid targets the group; works because we spawned `detached`.
    process.kill(-pid, sig);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ESRCH") return;
    try {
      child.kill(sig);
    } catch {
      // already dead
    }
  }
}

// ---------------------------------------------------------------------------

export async function executeFull(
  action: FullAction,
  sessionDir: string,
  iter: number,
  signal: AbortSignal,
): Promise<void> {
  if (action.execute.length === 0) {
    throw new ExecuteError(action.kind, null, "", "empty execute argv");
  }

  const [cmd, ...args] = action.execute;
  if (cmd === undefined) {
    throw new ExecuteError(action.kind, null, "", "empty execute argv");
  }

  const timeoutSeconds = (action.timeout_seconds ??
    PositiveSeconds(60)) as number;
  const timeoutMs = timeoutSeconds * 1000;

  const logPath = path.join(sessionDir, `iter-${String(iter)}.execute.log`);
  const logStream = createWriteStream(logPath, { flags: "a" });
  const writeLog = (line: string): void => {
    logStream.write(line);
  };
  writeLog(`$ ${action.execute.join(" ")}\n`);

  if (signal.aborted) {
    logStream.end();
    throw new ExecuteError(action.kind, null, "", "aborted before spawn");
  }

  const child = spawn(cmd, args, {
    env: { ...process.env },
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
  });

  const stderrBuf: CappedBuffer = { data: "", truncated: false, bytes: 0 };
  const stdoutBuf: CappedBuffer = { data: "", truncated: false, bytes: 0 };

  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");

  child.stdout.on("data", (chunk: string) => {
    appendCapped(stdoutBuf, chunk);
    writeLog(chunk);
  });
  child.stderr.on("data", (chunk: string) => {
    appendCapped(stderrBuf, chunk);
    writeLog(chunk);
  });

  const state = { timedOut: false, aborted: false, killedHard: false };

  const scheduleHardKill = (): void => {
    setTimeout(() => {
      if (child.exitCode === null && child.signalCode === null) {
        state.killedHard = true;
        killGroup(child, "SIGKILL");
      }
    }, KILL_GRACE_MS).unref();
  };

  const softKillTimer = setTimeout(() => {
    state.timedOut = true;
    verbose(`execute: timeout after ${timeoutSeconds}s (${action.kind})`);
    killGroup(child, "SIGTERM");
    scheduleHardKill();
  }, timeoutMs);

  const onAbort = (): void => {
    state.aborted = true;
    verbose(`execute: aborted (${action.kind})`);
    killGroup(child, "SIGTERM");
    scheduleHardKill();
  };
  signal.addEventListener("abort", onAbort, { once: true });

  interface ExitInfo {
    readonly code: number | null;
    readonly signalCode: NodeJS.Signals | null;
  }
  const exit: ExitInfo = await new Promise<ExitInfo>((resolve, reject) => {
    child.on("error", (err) => {
      clearTimeout(softKillTimer);
      signal.removeEventListener("abort", onAbort);
      reject(err);
    });
    child.on("close", (code, signalCode) => {
      clearTimeout(softKillTimer);
      signal.removeEventListener("abort", onAbort);
      resolve({ code, signalCode });
    });
  }).catch((err: unknown): ExitInfo => {
    writeLog(`\n[spawn error: ${String(err)}]\n`);
    return { code: null, signalCode: null };
  });

  await new Promise<void>((resolve) => {
    logStream.end(resolve);
  });

  if (state.aborted) {
    throw new ExecuteError(
      action.kind,
      exit.code,
      stderrBuf.data,
      `aborted (${action.kind})`,
    );
  }
  if (state.timedOut) {
    throw new ExecuteError(
      action.kind,
      exit.code,
      stderrBuf.data,
      `timeout after ${timeoutSeconds}s${state.killedHard ? " (SIGKILL)" : ""} (${action.kind})`,
    );
  }
  if (exit.code !== 0) {
    throw new ExecuteError(action.kind, exit.code, stderrBuf.data);
  }
}
