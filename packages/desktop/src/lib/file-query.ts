import { isCancelledError, rpc, type CancellableCall, type FileQueryResult } from "../ipc";

type Snapshot = {
  mode: "browse" | "sql";
  sql: string | null;
  result: FileQueryResult | null;
  elapsed: number | null;
  history: string[];
  error: string | null;
  status: "idle" | "running" | "cancelling" | "cancelled";
};

export class FileQuery {
  private state: Snapshot = {
    mode: "browse", sql: null, result: null, elapsed: null, history: [], error: null, status: "idle",
  };
  private listeners = new Set<() => void>();
  private nativeSessionId: string | null = null;
  private active: CancellableCall<FileQueryResult> | null = null;

  constructor(readonly repoId: string, readonly path: string, private rev: string | null) {}

  snapshot = () => this.state;

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  };

  setMode(mode: Snapshot["mode"]): void {
    this.publish({ mode });
  }

  setSql(sql: string): void {
    this.publish({ sql });
  }

  rebind(nativeSessionId: string | null): void {
    if (nativeSessionId === this.nativeSessionId) return;
    this.nativeSessionId = nativeSessionId;
    if (!this.active) return;
    this.active.cancel();
    this.active = null;
    this.publish({ status: "idle", error: "Repository services changed. Run the query again." });
  }

  startDefault(nativeSessionId: string, sql: string): void {
    if (this.state.sql !== null) return;
    this.setSql(sql);
    if (sql.trim()) void this.run(nativeSessionId, sql);
  }

  async run(nativeSessionId: string, sql: string): Promise<void> {
    const statement = sql.trim();
    if (!statement || this.active) return;
    this.rebind(nativeSessionId);
    const operation = rpc.fileQuery(this.repoId, this.path, statement, {
      rev: this.rev ?? undefined,
      native_session_id: nativeSessionId,
      max_rows: 1000,
    });
    this.active = operation;
    const began = performance.now();
    this.publish({ error: null, status: "running", result: null, elapsed: null });
    try {
      const result = await operation.promise;
      if (this.active !== operation) return;
      this.publish({
        result,
        status: "idle",
        elapsed: performance.now() - began,
        history: [statement, ...this.state.history.filter(previous => previous !== statement)].slice(0, 30),
      });
    } catch (error) {
      if (this.active !== operation) return;
      if (isCancelledError(error)) this.publish({ status: "cancelled" });
      else this.publish({ status: "idle", error: error instanceof Error ? error.message : String(error) });
    } finally {
      if (this.active === operation) this.active = null;
    }
  }

  cancel(): void {
    if (!this.active || this.state.status === "cancelling") return;
    this.active.cancel();
    this.publish({ status: "cancelling" });
  }

  private publish(patch: Partial<Snapshot>): void {
    this.state = { ...this.state, ...patch };
    for (const listener of this.listeners) listener();
  }
}

const sessions = new Map<string, FileQuery>();

export function fileQuery(repoId: string, path: string, rev: string | null): FileQuery {
  const key = JSON.stringify([repoId, path, rev]);
  let session = sessions.get(key);
  if (!session) {
    session = new FileQuery(repoId, path, rev);
    sessions.set(key, session);
  }
  return session;
}

export function retainFileQueryRepositories(repositories: Map<string, string | null>): void {
  for (const [key, session] of sessions) {
    session.rebind(repositories.get(session.repoId) ?? null);
    if (!repositories.has(session.repoId)) sessions.delete(key);
  }
}
