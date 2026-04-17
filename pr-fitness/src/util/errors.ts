/** Missing prerequisite (gh not installed, not authenticated). */
export class PreconditionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PreconditionError";
  }
}
