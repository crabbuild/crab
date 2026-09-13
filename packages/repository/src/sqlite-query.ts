import {
  defaultDataQuery,
  profileSqliteDataQuery,
  type QueryResult,
  type QuerySession,
  type QuerySource,
} from "./data-query";

type Pending = {
  reject: (reason: Error) => void;
  resolve: (result: QueryResult) => void;
};

export function createSqliteQuerySession(
  source: QuerySource,
  onProgress?: (loaded: number, total: number) => void,
) {
  if (!source.bytes)
    return Promise.reject(new Error("SQLite bytes were not loaded."));
  const sourceBytes = source.bytes;
  const worker = new Worker(
    new URL("./sqlite-query.worker.ts", import.meta.url),
    { name: "Crab SQLite query", type: "module" },
  );
  const pending = new Map<number, Pending>();
  let nextId = 0;
  let closed = false;
  let rejectOpen: (reason: Error) => void = () => undefined;

  const fail = (error: Error) => {
    for (const request of pending.values()) request.reject(error);
    pending.clear();
    rejectOpen(error);
  };
  const terminate = (message: string) => {
    if (closed) return;
    closed = true;
    worker.terminate();
    fail(new Error(message));
  };
  const run = (mode: "explain" | "query", sql: string) =>
    new Promise<QueryResult>((resolve, reject) => {
      if (closed) {
        reject(new Error("The SQLite session is closed."));
        return;
      }
      const id = ++nextId;
      pending.set(id, { reject, resolve });
      worker.postMessage({ type: "run", id, mode, sql });
    });

  return new Promise<QuerySession>((resolve, reject) => {
    rejectOpen = reject;
    worker.onerror = (event) =>
      terminate(event.message || "The SQLite query worker stopped.");
    worker.onmessage = (event: MessageEvent) => {
      const message = event.data as Record<string, unknown>;
      if (message.type === "ready") {
        rejectOpen = () => undefined;
        onProgress?.(source.size, source.size);
        const defaultRelation = String(message.defaultRelation);
        resolve({
          defaultQuery: defaultDataQuery(defaultRelation),
          relations: message.relations as QuerySession["relations"],
          sourceRows:
            typeof message.sourceRows === "number"
              ? message.sourceRows
              : undefined,
          cancel: async () => {
            terminate("SQLite query stopped.");
            return false;
          },
          close: async () => {
            if (closed) return;
            closed = true;
            worker.postMessage({ type: "close" });
            worker.terminate();
            fail(new Error("The SQLite session is closed."));
          },
          explain: (sql) => run("explain", sql),
          profileQuery: profileSqliteDataQuery,
          query: (sql) => run("query", sql),
        });
        return;
      }
      if (message.type === "error" && typeof message.id !== "number") {
        terminate(String(message.message));
        return;
      }
      if (typeof message.id !== "number") return;
      const request = pending.get(message.id);
      if (!request) return;
      pending.delete(message.id);
      if (message.type === "error")
        request.reject(new Error(String(message.message)));
      else request.resolve(message.result as QueryResult);
    };
    const bytes = sourceBytes.slice();
    worker.postMessage({ type: "open", bytes }, [bytes.buffer]);
  });
}
