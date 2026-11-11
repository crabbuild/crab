import { randomUUID } from "node:crypto";
import { rpcReply, type RpcFailure } from "./rpc-transport";

const WINDOW_BYTES = 1024 * 1024;
const MIME: Record<string, string> = {
  mp4: "video/mp4", webm: "video/webm", mov: "video/quicktime", mkv: "video/x-matroska", avi: "video/x-msvideo",
  mp3: "audio/mpeg", wav: "audio/wav", flac: "audio/flac", ogg: "audio/ogg", aac: "audio/aac", m4a: "audio/mp4", wma: "audio/x-ms-wma",
};

type RequestAgent = (method: string, params: Record<string, unknown>) => Promise<unknown>;
type Notify = (owner: number, id: string, error: RpcFailure) => void;

interface Opening {
  owner: number;
  repo: string;
  session: string;
  operation: string;
  cancelled: boolean;
  invalidated: boolean;
  task: Promise<unknown>;
  cleanupFailure?: unknown;
}

interface Resource {
  owner: number;
  repo: string;
  session: string;
  file: string;
  size: number;
  mime: string;
  closed: boolean;
  invalidated: boolean;
  closing?: Promise<void>;
  requests: Set<AbortController>;
  pending: Map<string, Promise<unknown>>;
}

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object") throw new Error("Invalid media response");
  return value as Record<string, unknown>;
}

function byteRange(header: string | null, size: number): [number, number] | null {
  if (header === null) return [0, size];
  const match = /^bytes=(\d*)-(\d*)$/i.exec(header.trim());
  if (!match || size === 0 || (!match[1] && !match[2])) return null;
  const first = match[1] ? Number(match[1]) : null;
  const last = match[2] ? Number(match[2]) : null;
  if ((first !== null && !Number.isSafeInteger(first)) || (last !== null && !Number.isSafeInteger(last))) return null;
  if (first === null) return last !== null && last > 0 ? [Math.max(0, size - last), size] : null;
  if (first >= size || (last !== null && last < first)) return null;
  return [first, last === null ? size : Math.min(size, last + 1)];
}

export class MediaResources {
  private resources = new Map<string, Resource>();
  private openings = new Set<Opening>();
  private stopped = false;

  constructor(private request: RequestAgent, private notify: Notify) {}

  async open(owner: number, value: unknown): Promise<{ media_id: string; url: string; size: number; kind: string }> {
    if (this.stopped) throw new Error("Media reads are closed");
    const params = record(value);
    if (typeof params.repo_id !== "string" || typeof params.path !== "string") throw new Error("Media requires a repository and path");
    if (typeof params.native_session_id !== "string") throw new Error("Media requires an available native session");
    const mime = MIME[params.path.split(".").pop()?.toLowerCase() ?? ""];
    if (!mime) throw new Error("Unsupported media container");
    const operation = typeof params.op_id === "string" ? params.op_id : randomUUID();
    const opening: Opening = { owner, repo: params.repo_id, session: params.native_session_id, operation, cancelled: false, invalidated: false, task: Promise.resolve() };
    this.openings.add(opening);
    const work = this.capture(opening, params, mime);
    opening.task = work;
    try { return await work; } finally { this.openings.delete(opening); }
  }

  private async capture(opening: Opening, params: Record<string, unknown>, mime: string) {
    const opened = record(await this.request("file_open", { ...params, op_id: opening.operation }));
    if (typeof opened.file_id !== "string") throw new Error("Media reader did not return a file identity");
    if (opening.cancelled || this.stopped || !["git", "crab", "lfs"].includes(String(opened.kind)) || typeof opened.size !== "number" || !Number.isSafeInteger(opened.size) || opened.size < 0) {
      try { if (!opening.invalidated) await this.request("file_close", { repo_id: opening.repo, file_id: opened.file_id, native_session_id: opening.session }); }
      catch (error) { opening.cleanupFailure = error; throw error; }
      throw new Error(opening.cancelled || this.stopped ? "Media preview closed" : "Media reader returned an invalid source description");
    }
    const id = randomUUID();
    this.resources.set(id, { owner: opening.owner, repo: opening.repo, session: opening.session, file: opened.file_id, size: opened.size, mime,
      closed: false, invalidated: false, requests: new Set(), pending: new Map() });
    return { media_id: id, url: `crab-content://media/${id}`, size: opened.size, kind: String(opened.kind) };
  }

  async close(owner: number, id: string): Promise<void> {
    const resource = this.resources.get(id);
    if (!resource) return;
    if (resource.owner !== owner) throw new Error("Media reader belongs to another window");
    if (resource.closing) return resource.closing;
    resource.closed = true;
    resource.closing = Promise.resolve().then(async () => {
      for (const controller of resource.requests) controller.abort();
      await Promise.allSettled(resource.pending.values());
      // A revoked session cannot accept captures. Its owner drains separately;
      // never send an old identity to a replacement while reads settle.
      if (!resource.invalidated) {
        try { await this.request("file_close", { repo_id: resource.repo, file_id: resource.file, native_session_id: resource.session }); }
        catch (error) { if (!resource.invalidated) throw error; }
      }
      this.resources.delete(id);
    });
    return resource.closing;
  }

  async closeOwner(owner: number): Promise<void> { await this.closeMatching(resource => resource.owner === owner); }
  async closeRepo(repo: string): Promise<void> { await this.closeMatching(resource => resource.repo === repo); }
  async closeAll(): Promise<void> { this.stopped = true; await this.closeMatching(() => true); }

  async invalidateRepo(repo: string): Promise<void> {
    const openings = [...this.openings].filter(opening => opening.repo === repo);
    for (const opening of openings) { opening.invalidated = true; opening.cancelled = true; }
    const resources = [...this.resources].filter(([, resource]) => resource.repo === repo);
    for (const [id, resource] of resources) {
      resource.invalidated = true; resource.closed = true;
      for (const controller of resource.requests) controller.abort();
      this.resources.delete(id);
      this.notify(resource.owner, id, { code: "NATIVE_SESSION_LOST", message: "Desktop services are unavailable. The media reader will reopen after recovery." });
    }
    await Promise.allSettled([...openings.map(opening => opening.task), ...resources.flatMap(([, resource]) => [...resource.pending.values()])]);
  }

  private async closeMatching(matches: (value: { owner: number; repo: string }) => boolean) {
    const opening = [...this.openings].filter(matches);
    for (const work of opening) work.cancelled = true;
    const cancellation = await Promise.allSettled(opening.filter(work => !work.invalidated).map(work => this.request("rpc/cancel", { repo_id: work.repo, op_id: work.operation, native_session_id: work.session })));
    // Opening work closes a late captured file before it resolves. Its original
    // caller owns the terminal failure; cleanup still waits for every capture.
    await Promise.allSettled(opening.map(work => work.task));
    const results = await Promise.allSettled([...this.resources].filter(([, resource]) => matches(resource)).map(([id, resource]) => this.close(resource.owner, id)));
    const cleanupFailure = opening.find(work => work.cleanupFailure !== undefined)?.cleanupFailure;
    if (cleanupFailure !== undefined) throw cleanupFailure;
    const failed = [...results, ...cancellation].find(result => result.status === "rejected");
    if (failed?.status === "rejected") throw failed.reason;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const id = url.pathname.slice(1);
    const resource = url.protocol === "crab-content:" && url.host === "media" && !url.username && !url.password && !url.search && !url.hash ? this.resources.get(id) : undefined;
    if (!resource || resource.closed) return new Response(null, { status: 410 });
    if (request.method !== "GET" && request.method !== "HEAD") return new Response(null, { status: 405, headers: { Allow: "GET, HEAD" } });
    const requestedRange = request.headers.get("range");
    // This capability never changes representation and publishes no validators.
    // Ignore unknown units, HEAD ranges and unprovable If-Range conditions.
    const rangeHeader = request.method === "GET" && requestedRange?.toLowerCase().startsWith("bytes=") && !request.headers.has("if-range") ? requestedRange : null;
    const range = byteRange(rangeHeader, resource.size);
    const headers: Record<string, string> = { "Content-Type": resource.mime, "Accept-Ranges": "bytes", "Cache-Control": "no-store",
      "Access-Control-Allow-Origin": "*", "Referrer-Policy": "no-referrer", "X-Content-Type-Options": "nosniff" };
    if (!range) return new Response(null, { status: 416, headers: { ...headers, "Content-Range": `bytes */${resource.size}` } });
    let [offset, end] = range;
    headers["Content-Length"] = String(end - offset);
    const status = rangeHeader === null ? 200 : 206;
    if (status === 206) headers["Content-Range"] = `bytes ${offset}-${end - 1}/${resource.size}`;
    if (request.method === "HEAD" || offset === end) return new Response(null, { status, headers });
    const abort = new AbortController();
    resource.requests.add(abort);
    const stop = () => abort.abort();
    request.signal.addEventListener("abort", stop, { once: true });
    if (request.signal.aborted) abort.abort();
    let finished = false;
    const finish = () => { finished = true; resource.requests.delete(abort); request.signal.removeEventListener("abort", stop); };
    const failure = async (error: unknown) => {
      if (abort.signal.aborted || resource.closed) return;
      const reply = await rpcReply(async () => { throw error; });
      if (!reply.ok) this.notify(resource.owner, id, reply.error);
    };
    let first: Uint8Array;
    try { first = await this.read(resource, offset, Math.min(WINDOW_BYTES, end - offset), abort.signal); }
    catch (error) { await failure(error); finish(); return new Response(null, { status: 502 }); }
    let initial: Uint8Array | null = first;
    let active: Promise<Uint8Array> | undefined;
    offset += first.length;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        abort.signal.addEventListener("abort", () => { if (!finished) controller.error(new Error("Media request stopped")); finish(); }, { once: true });
        if (abort.signal.aborted) { controller.error(new Error("Media request stopped")); finish(); }
      },
      pull: async controller => {
        try {
          if (abort.signal.aborted || resource.closed) throw new Error("Media request stopped");
          if (initial) { controller.enqueue(initial); initial = null; }
          else if (offset < end) {
            active = this.read(resource, offset, Math.min(WINDOW_BYTES, end - offset), abort.signal);
            const bytes = await active;
            active = undefined;
            offset += bytes.length;
            controller.enqueue(bytes);
          }
          if (offset === end && !initial) { controller.close(); finish(); }
        } catch (error) { await failure(error); if (!finished) controller.error(error); finish(); }
      },
      cancel: async () => {
        abort.abort(); finish();
        // A seek can cancel this response while another request uses the same
        // file. Await this query's terminal reply without draining other seeks.
        if (active) await Promise.allSettled([active]);
      },
    });
    return new Response(body, { status, headers });
  }

  private async read(resource: Resource, offset: number, length: number, signal: AbortSignal): Promise<Uint8Array> {
    signal.throwIfAborted();
    if (resource.closed) throw new Error("Media preview closed");
    const operation = randomUUID();
    const work = this.request("read_range", { repo_id: resource.repo, file_id: resource.file, native_session_id: resource.session, offset, length, op_id: operation });
    resource.pending.set(operation, work);
    const cancel = () => { if (!resource.invalidated) void this.request("rpc/cancel", { repo_id: resource.repo, op_id: operation, native_session_id: resource.session }).catch(() => undefined); };
    signal.addEventListener("abort", cancel, { once: true });
    try {
      const result = record(await work);
      if (result.offset !== offset || result.length !== length || result.file_size !== resource.size || typeof result.bytes_b64 !== "string") throw new Error("Media reader returned a mismatched range");
      const bytes = Buffer.from(result.bytes_b64, "base64");
      if (bytes.length !== length) throw new Error("Media reader returned incomplete bytes");
      return bytes;
    } finally { resource.pending.delete(operation); signal.removeEventListener("abort", cancel); }
  }
}
