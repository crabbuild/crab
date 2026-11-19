import { createServer, request, type ClientRequest } from "node:http";

export async function resultsStorageProxy(endpoint: string) {
  const target = new URL(endpoint);
  if (
    target.protocol !== "http:" ||
    !["127.0.0.1", "localhost", "[::1]"].includes(target.hostname)
  ) {
    throw new Error(
      "Storage failure proof requires an owned loopback HTTP fixture",
    );
  }
  let fault: { path: string; disconnected: boolean } | null = null;
  let observed: { method: string; status: number } | null = null;
  const pending = new Set<ClientRequest>();
  const held = new Set<{ destroy(): void }>();
  const readers = { active: 0, peak: 0, started: 0, finished: 0 };
  const server = createServer((incoming, outgoing) => {
    const url = new URL(incoming.url ?? "/", target);
    const selected = fault && url.pathname.endsWith(fault.path);
    if (fault?.disconnected) {
      outgoing.destroy();
      return;
    }
    // Preserve the signed Host and all request bytes. Only the transport
    // destination changes; successful responses still come from real storage.
    const upstream = request(
      url,
      { method: incoming.method, headers: incoming.headers },
      (response) => {
        response.on("error", () => outgoing.destroy());
        if (selected) {
          observed = {
            method: incoming.method ?? "",
            status: response.statusCode ?? 0,
          };
          held.add(outgoing);
          response.pause();
          return;
        }
        outgoing.writeHead(response.statusCode ?? 502, response.headers);
        response.pipe(outgoing);
      },
    );
    pending.add(upstream);
    const read = incoming.method === "GET" || incoming.method === "HEAD";
    if (read) {
      readers.started++;
      readers.active++;
      readers.peak = Math.max(readers.peak, readers.active);
    }
    let upstreamClosed = false;
    let downstreamClosed = false;
    let released = false;
    const finish = () => {
      if (read && !released && upstreamClosed && downstreamClosed) {
        released = true;
        readers.active--;
        readers.finished++;
      }
    };
    // Both response lifetimes must terminate. Upstream closure alone cannot
    // release a reader whose downstream body is intentionally held open.
    upstream.once("close", () => {
      pending.delete(upstream);
      upstreamClosed = true;
      finish();
    });
    upstream.on("error", () => outgoing.destroy());
    incoming.on("error", () => upstream.destroy());
    outgoing.once("close", () => {
      held.delete(outgoing);
      downstreamClosed = true;
      upstream.destroy();
      finish();
    });
    incoming.pipe(upstream);
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("Storage proxy has no TCP address");
  return {
    endpoint: `http://127.0.0.1:${address.port}`,
    hold(hash: string) {
      if (held.size || !/^[0-9a-f]{64}$/.test(hash))
        throw new Error("Invalid storage hold");
      fault = {
        path: `/workflow/results/objects/${hash}`,
        disconnected: false,
      };
      observed = null;
    },
    get held() {
      return held.size;
    },
    get observed() {
      return observed;
    },
    get readers() {
      return { ...readers };
    },
    disconnect() {
      if (!fault || !held.size)
        throw new Error("No in-flight storage response to disconnect");
      fault.disconnected = true;
      for (const upstream of pending) upstream.destroy();
      server.closeAllConnections();
    },
    resume() {
      fault = null;
    },
    async close() {
      for (const upstream of pending) upstream.destroy();
      const closing = new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
      server.closeAllConnections();
      await closing;
    },
  };
}
