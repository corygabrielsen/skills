/** Missing prerequisite (gh not installed, not authenticated). */
export class PreconditionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PreconditionError";
  }
}

/** A `gh` subprocess exited with non-zero or produced invalid output. */
export class GitHubError extends Error {
  readonly command: string;
  readonly exitCode: number | null;
  readonly stderr: string;

  constructor(command: string, exitCode: number | null, stderr: string) {
    const trimmed = stderr.trim();
    const detail = trimmed ? `\n  ${trimmed.split("\n").join("\n  ")}` : "";
    super(`gh failed (exit ${String(exitCode ?? "?")}): ${command}${detail}`);
    this.name = "GitHubError";
    this.command = command;
    this.exitCode = exitCode;
    this.stderr = stderr;
  }
}
