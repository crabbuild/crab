/** Keep a repository alive until its admitted transfers settle. */
export class FileTransferOwner {
  private pending = new Map<string, Set<Promise<void>>>();
  private retiring = new Set<string>();
  private stopping = false;

  constructor(private release: (repoId: string) => Promise<unknown>) {}

  async run<T>(repoId: string, work: () => Promise<T>): Promise<T> {
    if (this.stopping || this.retiring.has(repoId)) throw new Error("Repository file services are closing. Reopen the repository to transfer files.");
    let complete!: () => void;
    const completion = new Promise<void>(resolve => { complete = resolve; });
    const pending = this.pending.get(repoId) ?? new Set<Promise<void>>();
    this.pending.set(repoId, pending);
    pending.add(completion);
    try {
      return await work();
    } finally {
      pending.delete(completion);
      if (pending.size === 0) this.pending.delete(repoId);
      complete();
    }
  }

  async stop(): Promise<void> {
    this.stopping = true;
    await Promise.all([...this.pending.values()].flatMap(pending => [...pending]));
  }

  async close(repoId: string): Promise<unknown> {
    this.retiring.add(repoId);
    try {
      // Retire admission before joining, so view teardown cannot release a
      // native handle still owned by publication or admit a new late intent.
      await Promise.all(this.pending.get(repoId) ?? []);
      return await this.release(repoId);
    } finally {
      this.retiring.delete(repoId);
    }
  }
}
