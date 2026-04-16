/**
 * Session management: lock acquisition, history persistence, stale-session GC.
 *
 * Session directory layout (`/tmp/converge/{sessionId}/`):
 *   lock                — pid of owning process (atomic O_EXCL create)
 *   history.jsonl       — one IterLog JSON per line, append-only
 *   iter-{i}.execute.log — captured stdout/stderr for full-automation actions
 *   exit.json           — terminal HaltReport (written once on halt)
 *
 * Locking: `fs.open(lock, "wx")` is atomic across Linux filesystems. On
 * EEXIST, we read the PID, probe liveness via `process.kill(pid, 0)`, and
 * reclaim stale locks. Bounded retry (5 × 100ms) prevents thundering herds.
 *
 * GC: best-effort sweep of `/tmp/converge/` on startup. Any session dir
 * without a live lock and with mtime older than `maxAgeMs` is removed.
 */

import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import type { IterLog } from "./types/index.js";
import { LockHeldError, PreconditionError } from "./util/errors.js";
import { verbose } from "./util/log.js";
import { sleep } from "./util/sleep.js";

const SESSIONS_ROOT = path.join(os.tmpdir(), "converge");
const LOCK_MAX_RETRIES = 5;
const LOCK_RETRY_MS = 100;

export interface SessionHandle {
  readonly dir: string;
  readonly sessionId: string;
  /** Live, append-only history. Starts populated from history.jsonl if any. */
  readonly history: IterLog[];
  release(): Promise<void>;
}

// ---------------------------------------------------------------------------

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return err instanceof Error && "code" in err;
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    if (isNodeError(err) && err.code === "EPERM") {
      // Permission denied means the process exists but belongs to someone
      // else; treat as alive to avoid reclaiming a stranger's lock.
      return true;
    }
    return false;
  }
}

function parsePidStrict(raw: string): number | null {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const n = Number.parseInt(trimmed, 10);
  if (!Number.isInteger(n) || n <= 0) return null;
  return n;
}

async function loadHistory(dir: string): Promise<IterLog[]> {
  const historyPath = path.join(dir, "history.jsonl");
  let raw: string;
  try {
    raw = await fs.readFile(historyPath, "utf8");
  } catch (err) {
    if (isNodeError(err) && err.code === "ENOENT") return [];
    throw err;
  }
  const history: IterLog[] = [];
  for (const line of raw.split("\n")) {
    if (line.trim() === "") continue;
    const parsed: unknown = JSON.parse(line);
    if (!isIterLog(parsed)) {
      throw new PreconditionError(
        `corrupt history.jsonl line at ${historyPath}`,
      );
    }
    history.push(parsed);
  }
  return history;
}

function isIterLog(v: unknown): v is IterLog {
  if (typeof v !== "object" || v === null) return false;
  const o = v as Record<string, unknown>;
  if (typeof o["iter"] !== "number") return false;
  if (typeof o["score"] !== "number") return false;
  const summary = o["action_summary"];
  if (typeof summary !== "object" || summary === null) return false;
  const s = summary as Record<string, unknown>;
  return typeof s["kind"] === "string" && typeof s["automation"] === "string";
}

// ---------------------------------------------------------------------------

async function acquireLock(dir: string): Promise<() => Promise<void>> {
  const lockPath = path.join(dir, "lock");
  const pid = process.pid;

  for (let attempt = 0; attempt < LOCK_MAX_RETRIES; attempt++) {
    try {
      const handle = await fs.open(lockPath, "wx");
      try {
        await handle.writeFile(String(pid));
      } finally {
        await handle.close();
      }
      return async (): Promise<void> => {
        try {
          await fs.unlink(lockPath);
        } catch (err) {
          if (isNodeError(err) && err.code === "ENOENT") return;
          throw err;
        }
      };
    } catch (err) {
      if (!isNodeError(err) || err.code !== "EEXIST") throw err;
    }

    // Lock exists. Check owner liveness.
    let existingPid: number | null = null;
    try {
      const raw = await fs.readFile(lockPath, "utf8");
      existingPid = parsePidStrict(raw);
    } catch (readErr) {
      if (isNodeError(readErr) && readErr.code === "ENOENT") {
        // Lock vanished between create-fail and read; retry.
        continue;
      }
      throw readErr;
    }

    if (existingPid === null) {
      // Corrupt lock file — treat as stale and reclaim.
      verbose(`session: corrupt lock at ${lockPath}, reclaiming`);
      await fs.unlink(lockPath).catch(() => undefined);
      continue;
    }

    if (isAlive(existingPid)) {
      throw new LockHeldError(existingPid, lockPath);
    }

    verbose(`session: stale lock (pid ${existingPid}) at ${lockPath}`);
    await fs.unlink(lockPath).catch(() => undefined);

    if (attempt + 1 < LOCK_MAX_RETRIES) {
      await sleep(LOCK_RETRY_MS);
    }
  }

  // Exhausted retries — treat as contended.
  let finalPid = 0;
  try {
    const raw = await fs.readFile(lockPath, "utf8");
    finalPid = parsePidStrict(raw) ?? 0;
  } catch {
    // ignore
  }
  throw new LockHeldError(finalPid, lockPath);
}

// ---------------------------------------------------------------------------

export async function openSession(sessionId: string): Promise<SessionHandle> {
  if (sessionId === "" || sessionId.includes("/") || sessionId.includes("..")) {
    throw new PreconditionError(`invalid sessionId: ${sessionId}`);
  }

  const dir = path.join(SESSIONS_ROOT, sessionId);
  await fs.mkdir(dir, { recursive: true });

  const release = await acquireLock(dir);

  let history: IterLog[];
  try {
    history = await loadHistory(dir);
  } catch (err) {
    // Lock is held; release before rethrowing.
    await release().catch(() => undefined);
    throw err;
  }

  return {
    dir,
    sessionId,
    history,
    release,
  };
}

// ---------------------------------------------------------------------------

export async function gcStaleSessions(maxAgeMs: number): Promise<void> {
  let entries: string[];
  try {
    entries = await fs.readdir(SESSIONS_ROOT);
  } catch (err) {
    if (isNodeError(err) && err.code === "ENOENT") return;
    throw err;
  }

  const now = Date.now();

  await Promise.all(
    entries.map(async (name) => {
      const dir = path.join(SESSIONS_ROOT, name);
      try {
        const st = await fs.stat(dir);
        if (!st.isDirectory()) return;
        if (now - st.mtimeMs < maxAgeMs) return;

        const lockPath = path.join(dir, "lock");
        try {
          const raw = await fs.readFile(lockPath, "utf8");
          const pid = parsePidStrict(raw);
          if (pid !== null && isAlive(pid)) return;
        } catch (err) {
          if (!isNodeError(err) || err.code !== "ENOENT") return;
          // ENOENT is fine — no lock means nobody owns it.
        }

        await fs.rm(dir, { recursive: true, force: true });
        verbose(`session: gc removed ${dir}`);
      } catch (err) {
        verbose(`session: gc skipped ${dir} (${String(err)})`);
      }
    }),
  );
}

// ---------------------------------------------------------------------------

/**
 * Append an IterLog to history.jsonl and the in-memory array atomically
 * w.r.t. the caller (single write, fsync deferred to kernel).
 */
export async function appendHistory(
  handle: SessionHandle,
  entry: IterLog,
): Promise<void> {
  const line = `${JSON.stringify({
    iter: entry.iter,
    score: entry.score as number,
    action_summary: entry.action_summary,
  })}\n`;
  await fs.appendFile(path.join(handle.dir, "history.jsonl"), line, "utf8");
  handle.history.push(entry);
}
