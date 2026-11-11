import { realpath, stat } from "node:fs/promises";
import * as path from "node:path";
import { Worker } from "node:worker_threads";

interface Owner {
  ready: Promise<void>;
  closed: Promise<void>;
  close: () => Promise<void>;
  listeners: Set<() => void>;
}

/** Own local metadata monitors outside Electron's UI thread. */
export class LocalFileWatchers {
  private owners = new Map<string, Owner>();
  private all = new Set<Owner>();
  private stopped = false;

  async watch(root: string, changed: () => void): Promise<() => Promise<void>> {
    if (!path.isAbsolute(root)) throw new Error("The watch target must be an absolute local directory.");
    const physical = await realpath(root);
    if (!(await stat(physical)).isDirectory()) throw new Error("The watch target is not a directory.");
    if (this.stopped) throw new Error("Filesystem watchers are shutting down.");
    let current = this.owners.get(physical);
    if (!current) {
      current = this.open(physical);
      this.owners.set(physical, current);
      this.all.add(current);
    }
    const owner = current, listener = () => changed();
    owner.listeners.add(listener);
    let stopping: Promise<void> | undefined;
    const stop = () => stopping ??= Promise.resolve().then(async () => {
      owner.listeners.delete(listener);
      if (!owner.listeners.size) await owner.close();
    });
    try { await owner.ready; }
    catch (error) { await owner.closed; owner.listeners.delete(listener); throw error; }
    return stop;
  }

  async stopAll(): Promise<void> {
    this.stopped = true;
    const results = await Promise.allSettled([...this.all].map(owner => {
      owner.listeners.clear();
      return owner.close();
    }));
    const errors = results.filter(result => result.status === "rejected").map(result => result.reason);
    if (errors.length) throw new AggregateError(errors, "Filesystem watcher cleanup failed.");
  }

  private open(root: string): Owner {
    const worker = new Worker(path.join(__dirname, "local-file-watcher-worker.cjs"), { workerData: root });
    const listeners = new Set<() => void>();
    let failure: Error | undefined, exited = false, closing: Promise<void> | undefined;
    let started = false, resolveReady!: () => void, rejectReady!: (error: Error) => void;
    const ready = new Promise<void>((resolve, reject) => { resolveReady = resolve; rejectReady = reject; });
    const closed = new Promise<void>(resolve => {
      worker.once("error", error => { failure = error; rejectReady(error); });
      worker.once("exit", code => {
        exited = true;
        if (code && !failure) failure = new Error(`Filesystem watcher exited with status ${code}.`);
        if (!started) rejectReady(failure ?? new Error("Filesystem watcher stopped before starting."));
        if (this.owners.get(root) === owner) this.owners.delete(root);
        this.all.delete(owner);
        resolve();
      });
    });
    worker.on("message", message => {
      if (message === "ready") { started = true; resolveReady(); }
      else if (message === "changed") for (const changed of listeners) changed();
    });
    const owner: Owner = { ready, closed, listeners, close: () => {
      if (closing) return closing;
      // A new subscription cannot attach to an owner already draining. Keep
      // that owner in all until exit so application shutdown still joins it.
      if (this.owners.get(root) === owner) this.owners.delete(root);
      closing = Promise.resolve().then(async () => {
        // A worker owns its metadata scan. Stop waits for that scan to release
        // directory handles; application exit must not abandon its owner.
        if (!exited) worker.postMessage("stop");
        await closed;
        if (failure) throw failure;
      });
      return closing;
    } };
    return owner;
  }
}

export const localFileWatchers = new LocalFileWatchers();
