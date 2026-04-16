/**
 * Abortable async sleep.
 *
 * Resolves after `ms` milliseconds. If `signal` is provided and fires
 * (either already aborted or during the sleep), the returned promise rejects
 * with `DOMException("Aborted", "AbortError")` — matching the spec Node uses
 * for AbortController everywhere else.
 */

export function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    if (signal?.aborted) {
      reject(new DOMException("Aborted", "AbortError"));
      return;
    }

    const onAbort = (): void => {
      clearTimeout(timer);
      reject(new DOMException("Aborted", "AbortError"));
    };

    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);

    signal?.addEventListener("abort", onAbort, { once: true });
  });
}
