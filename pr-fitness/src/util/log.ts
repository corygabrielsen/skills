let quiet = false;

export function setQuiet(value: boolean): void {
  quiet = value;
}

export function log(message: string): void {
  if (!quiet) {
    process.stderr.write(`pr-fitness: ${message}\n`);
  }
}
