/**
 * Thin wrapper around the `gh` CLI.
 *
 * Single point of subprocess execution. Mock this module in tests
 * to make everything deterministic.
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { GitHubError, PreconditionError } from "./errors.js";

const exec = promisify(execFile);

/** Run `gh` with arguments and return parsed JSON. */
export async function gh<T>(args: readonly string[]): Promise<T> {
  try {
    const { stdout } = await exec("gh", args, {
      maxBuffer: 10 * 1024 * 1024,
    });
    return JSON.parse(stdout) as T;
  } catch (error: unknown) {
    if (isExecError(error) && error.code === "ENOENT") {
      throw new PreconditionError(
        "gh not found (install: https://cli.github.com)",
      );
    }
    if (isExecError(error)) {
      throw new GitHubError(
        `gh ${args.join(" ")}`,
        error.code !== undefined ? Number(error.code) : null,
        error.stderr ?? "",
      );
    }
    throw error;
  }
}

interface ExecError {
  code?: string | number;
  stderr?: string;
}

function isExecError(error: unknown): error is ExecError {
  return typeof error === "object" && error !== null && "code" in error;
}
