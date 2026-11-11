import type { App } from "electron";

/** Prevents repeated quit requests from bypassing asynchronous owner cleanup. */
export function registerAppShutdown(app: Pick<App, "on" | "exit">, owners: ReadonlyArray<() => unknown | Promise<unknown>>, afterOwners: () => unknown | Promise<unknown>): void {
  let shuttingDown = false;
  app.on("will-quit", event => {
    event.preventDefault();
    if (shuttingDown) return;
    shuttingDown = true;
    // Invoke every owner even if another throws, and await every closure
    // before app.exit bypasses Electron's remaining quit events.
    void Promise.allSettled(owners.map(owner => Promise.resolve().then(owner))).then(async results => {
      const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      for (const error of errors) console.error("[app] shutdown failed:", error.reason);
      // Durable receipts stay writable until native and transport owners drain.
      let exitCode = errors.length ? 1 : 0;
      try { await afterOwners(); }
      catch (error) { console.error("[app] shutdown finalization failed:", error); exitCode = 1; }
      app.exit(exitCode);
    });
  });
}
