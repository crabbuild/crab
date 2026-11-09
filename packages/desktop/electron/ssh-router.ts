import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { TextDecoder } from "node:util";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteCache } from "./ssh-cache";

export interface RepoTransport { type: "local" | "remote"; profileId?: string }
export interface RpcRouterOptions {
  /** Metadata/control timeout; explicit values also bound owned content operations. */
  requestTimeoutMs?: number;
  maxQueuedCalls?: number;
}

interface ChannelOwner {
  profileId: string;
  channel: ClientChannel;
  parts: Buffer[];
  bytes: number;
  closed: boolean;
  draining?: Error;
}
interface PendingRequest {
  owner: ChannelOwner;
  method: string;
  operation?: { op_id: string; repo_id?: string };
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer?: ReturnType<typeof setTimeout>;
  timeout?: Error;
  cancelRequested?: boolean;
}
interface QueuedCall {
  method: string;
  params: unknown;
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}
interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number | null;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}
type BatchReply = { result?: unknown; error?: { code: number; message: string; data?: unknown } };

// These methods own bounded content work and acknowledge cancellation only
// after their readers drain. The generic 30-second metadata timer must not
// discard a multi-GiB verification or materialization halfway through it.
const CONTENT_METHODS = new Set([
  "file_open", "file_diff", "file_chunks", "file_materialize", "read_range", "read_text", "read_bytes",
  "preview_parquet", "preview_safetensors", "preview_npy", "preview_npz", "preview_zip",
  "preview_notebook_meta", "preview_torch_checkpoint",
]);
const CACHEABLE_READ_METHODS = new Set([
  "git_status_porcelain", "git_log", "list_refs", "repo_summary", "hydration_status",
]);

export function parseRepoId(repoId: string): { profileId: string; remotePath: string } | null {
  if (!repoId.startsWith("ssh://")) return null;
  const value = repoId.slice(6);
  const slash = value.indexOf("/");
  return slash < 0 ? { profileId: value, remotePath: "/" } : { profileId: value.slice(0, slash), remotePath: value.slice(slash) };
}
export function isCacheableReadMethod(method: string): boolean { return CACHEABLE_READ_METHODS.has(method); }

export interface BandwidthStats { bytesIn: number; bytesOut: number }

export class RpcRouter extends EventEmitter {
  private transports = new Map<string, RepoTransport>();
  private channels = new Map<string, ChannelOwner>();
  private pending = new Map<number, PendingRequest>();
  private queues = new Map<string, QueuedCall[]>();
  private replays = new Map<string, Promise<void>>();
  private nextId = 1;
  private bandwidthCounters = new Map<string, BandwidthStats>();
  private remoteCache: RemoteCache | null = null;
  private readonly requestTimeoutMs: number;
  private readonly contentTimeoutMs?: number;
  private readonly maxQueuedCalls: number;

  constructor(
    private readonly sessionManager: SshSessionManager,
    private readonly localAgent: { request(method: string, params: unknown): Promise<unknown> },
    options: RpcRouterOptions = {},
  ) {
    super();
    this.requestTimeoutMs = options.requestTimeoutMs ?? 30_000;
    this.contentTimeoutMs = options.requestTimeoutMs;
    this.maxQueuedCalls = options.maxQueuedCalls ?? 50;
  }

  setCache(cache: RemoteCache): void { this.remoteCache = cache; }
  registerTransport(repoId: string, transport: RepoTransport): void { this.transports.set(repoId, transport); }
  unregisterTransport(repoId: string): void { this.transports.delete(repoId); }

  async route(repoId: string, method: string, params: unknown): Promise<unknown> {
    const parsed = parseRepoId(repoId);
    if (parsed) return this.routeToRemote(parsed.profileId, method, params);
    const transport = this.transports.get(repoId);
    if (transport?.type === "remote" && transport.profileId) return this.routeToRemote(transport.profileId, method, params);
    return this.localAgent.request(method, params);
  }

  /** Independent calls share single-request framing, ownership and errors. */
  async batchRpc(repoId: string, calls: Array<{ method: string; params: unknown }>): Promise<BatchReply[]> {
    const results: BatchReply[] = new Array(calls.length);
    let next = 0;
    // Workspace polling must not spawn a Git child for every repository at once.
    await Promise.all(Array.from({ length: Math.min(4, calls.length) }, async () => {
      while (next < calls.length) {
        const index = next++;
        const call = calls[index];
        try { results[index] = { result: await this.route(repoId, call.method, call.params) }; }
        catch (error) {
          const detail = error as { code?: unknown; message?: unknown; data?: unknown } | null;
          const failure: NonNullable<BatchReply["error"]> = {
            code: typeof detail?.code === "number" ? detail.code : -32000,
            message: typeof detail?.message === "string" ? detail.message : String(error),
          };
          if (detail && "data" in detail) failure.data = detail.data;
          results[index] = { error: failure };
        }
      }
    }));
    return results;
  }

  attachChannel(profileId: string, channel: ClientChannel): void {
    const previous = this.channels.get(profileId);
    if (previous?.channel === channel) return;
    if (previous) this.failChannel(previous, Object.assign(new Error("Remote agent channel replaced"), { code: "SSH_CHANNEL_REPLACED" }));
    const owner: ChannelOwner = { profileId, channel, parts: [], bytes: 0, closed: false };
    this.channels.set(profileId, owner);
    if (!this.bandwidthCounters.has(profileId)) this.resetBandwidth(profileId);
    const onData = (data: Buffer) => {
      if (this.channels.get(profileId) !== owner || owner.closed) return;
      this.bandwidthCounters.get(profileId)!.bytesIn += data.length;
      this.handleChannelData(owner, data);
    };
    const onError = (error: Error) => this.failChannel(owner, error);
    const onEnd = () => this.failChannel(owner, Object.assign(new Error(owner.bytes ? "Remote agent ended with an incomplete RPC frame" : "Remote agent channel closed"), { code: owner.bytes ? "SSH_PROTOCOL_ERROR" : "SSH_CHANNEL_CLOSED" }));
    const onClose = () => {
      onEnd();
      channel.removeListener("data", onData);
      channel.removeListener("error", onError);
      channel.removeListener("end", onEnd);
      channel.removeListener("close", onClose);
    };
    channel.on("data", onData);
    channel.on("error", onError);
    channel.on("end", onEnd);
    channel.on("close", onClose);
  }

  private failChannel(owner: ChannelOwner, error: Error): void {
    if (owner.closed) return;
    owner.closed = true;
    owner.parts = []; owner.bytes = 0;
    if (this.channels.get(owner.profileId) === owner) this.channels.delete(owner.profileId);
    for (const [id, request] of this.pending) {
      if (request.owner !== owner) continue;
      clearTimeout(request.timer);
      this.pending.delete(id);
      request.reject(error);
    }
    // EOF lets the native agent cancel and join reads, then close its SDK/DBs.
    owner.channel.end();
  }

  replayQueue(profileId: string): Promise<void> {
    const active = this.replays.get(profileId);
    if (active) return active;
    const queue = this.queues.get(profileId);
    if (!queue?.length) return Promise.resolve();
    const work = (async () => {
      // Unsent calls remain cancellable while another replay is in flight.
      while (queue.length) {
        const call = queue.shift()!;
        try { call.resolve(await this.sendToRemoteAgent(profileId, call.method, call.params)); }
        catch (error) { call.reject(error instanceof Error ? error : new Error(String(error))); }
      }
    })().finally(() => {
      if (this.queues.get(profileId) === queue) this.queues.delete(profileId);
      this.replays.delete(profileId);
    });
    this.replays.set(profileId, work);
    return work;
  }
  clearQueue(profileId: string): void {
    for (const call of this.queues.get(profileId)?.splice(0) ?? []) call.reject(new Error("Connection lost permanently"));
    this.queues.delete(profileId);
  }
  getBandwidth(profileId: string): BandwidthStats { return this.bandwidthCounters.get(profileId) ?? { bytesIn: 0, bytesOut: 0 }; }
  resetBandwidth(profileId: string): void { this.bandwidthCounters.set(profileId, { bytesIn: 0, bytesOut: 0 }); }

  private async routeToRemote(profileId: string, method: string, params: unknown): Promise<unknown> {
    const status = this.sessionManager.getStatus(profileId);
    if (method === "rpc/cancel" && params && typeof params === "object" && "op_id" in params) {
      const queue = this.queues.get(profileId);
      const index = queue?.findIndex(call => call.params && typeof call.params === "object" && "op_id" in call.params && call.params.op_id === params.op_id) ?? -1;
      if (queue && index >= 0) {
        const [call] = queue.splice(index, 1);
        call.reject(Object.assign(new Error("Queued operation cancelled"), { code: 1010, data: { kind: "op_cancelled" } }));
        return { ok: true };
      }
    }
    if (status === "reconnecting") {
      // Only explicitly cacheable metadata can be replayed. Captured sources,
      // cancellation signals and unknown mutations belong to one agent session.
      if (!isCacheableReadMethod(method)) throw Object.assign(new Error("Connection lost: live operations are unavailable during reconnection"), { code: "SSH_RECONNECTING" });
      const repoId = extractRepoId(params);
      const cached = repoId ? this.remoteCache?.getCached(`${repoId}:${method}`) : null;
      if (cached) {
        const data = cached.data;
        return { ...(data !== null && typeof data === "object" && !Array.isArray(data) ? data : { data }), __stale: true };
      }
      let queue = this.queues.get(profileId);
      if (!queue) { queue = []; this.queues.set(profileId, queue); }
      if (queue.length >= this.maxQueuedCalls) queue.shift()?.reject(new Error("Call dropped: reconnection queue overflow"));
      return new Promise((resolve, reject) => queue.push({ method, params, resolve, reject }));
    }
    if (status !== "connected") throw new Error(`SSH connection not available (status: ${status})`);
    return this.sendToRemoteAgent(profileId, method, params);
  }

  sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    const connection = this.sessionManager.getConnection(profileId);
    const owner = this.channels.get(profileId);
    if (!owner || owner.closed || owner.channel !== connection?.channels.agent) return Promise.reject(new Error(`No agent channel for profile: ${profileId}`));
    const content = CONTENT_METHODS.has(method);
    // Release must finish even if the initiating view/profile deadline expires.
    const timeout = method === "file_close" || method === "close" ? undefined
      : connection.profile?.operationTimeoutMs ?? (content ? this.contentTimeoutMs : this.requestTimeoutMs);
    const fields = params !== null && typeof params === "object" && !Array.isArray(params) ? params as Record<string, unknown> : null;
    const enriched = content && fields && typeof fields.op_id !== "string" ? { ...fields, op_id: randomUUID() } : params;
    return this.send(owner, method, enriched, timeout);
  }

  private send(owner: ChannelOwner, method: string, params: unknown, timeoutMs: number | undefined): Promise<unknown> {
    if (owner.closed || owner.draining) return Promise.reject(owner.draining ?? new Error("Remote agent channel closed"));
    const fields = params !== null && typeof params === "object" && !Array.isArray(params) ? params as Record<string, unknown> : null;
    const operation = method !== "rpc/cancel" && typeof fields?.op_id === "string"
      ? { op_id: fields.op_id, ...(typeof fields.repo_id === "string" ? { repo_id: fields.repo_id } : {}) } : undefined;
    if (method === "rpc/cancel") {
      for (const request of this.pending.values()) if (request.owner === owner && request.operation?.op_id === fields?.op_id) request.cancelRequested = true;
    }
    const id = this.nextId++;
    const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      const request: PendingRequest = { owner, method, operation, resolve, reject };
      this.pending.set(id, request);
      if (timeoutMs !== undefined) request.timer = setTimeout(() => {
        const error = Object.assign(new Error(`RPC timeout: ${method} did not respond within ${timeoutMs}ms`), { code: "SSH_RPC_TIMEOUT" });
        if (!operation) { this.pending.delete(id); reject(error); return; }
        request.timeout = error;
        // Signalling is not completion. Keep the original request until its
        // native terminal reply, preserving integrity errors and raced success.
        void this.send(owner, "rpc/cancel", operation, timeoutMs).catch(error => {
          if (owner.closed || owner.draining) return;
          owner.draining = error instanceof Error ? error : new Error(String(error));
          for (const pending of this.pending.values()) if (pending.owner === owner) clearTimeout(pending.timer);
          owner.channel.end();
        });
      }, timeoutMs);
      const counter = this.bandwidthCounters.get(owner.profileId);
      if (counter) counter.bytesOut += Buffer.byteLength(frame);
      owner.channel.write(frame, error => {
        if (!error) return;
        this.failChannel(owner, error);
      });
    });
  }

  private handleChannelData(owner: ChannelOwner, data: Buffer): void {
    let start = 0;
    while (start < data.length && !owner.closed) {
      const end = data.indexOf(10, start);
      const part = data.subarray(start, end < 0 ? data.length : end);
      owner.parts.push(part); owner.bytes += part.length;
      if (end < 0) return;
      const bytes = Buffer.concat(owner.parts, owner.bytes);
      owner.parts = []; owner.bytes = 0;
      try {
        const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        if (text.trim()) this.processLine(owner, text);
      } catch (error) {
        this.failChannel(owner, Object.assign(new Error(`Invalid remote RPC frame: ${error instanceof Error ? error.message : String(error)}`), { code: "SSH_PROTOCOL_ERROR" }));
      }
      start = end + 1;
    }
  }

  private processLine(owner: ChannelOwner, line: string): void {
    const frame: unknown = JSON.parse(line);
    if (!frame || typeof frame !== "object" || Array.isArray(frame) || !("jsonrpc" in frame) || frame.jsonrpc !== "2.0") throw new Error("Expected one JSON-RPC 2.0 frame");
    if ("method" in frame && typeof frame.method === "string" && !("id" in frame)) {
      this.emit("notification", { profileId: owner.profileId, method: frame.method, params: "params" in frame ? frame.params : undefined });
      return;
    }
    if (!("id" in frame) || (typeof frame.id !== "number" && frame.id !== null)) throw new Error("Invalid response ID");
    const response = frame as JsonRpcResponse;
    if (response.id === null) throw new Error(response.error?.message ?? "Uncorrelated remote error");
    const request = this.pending.get(response.id);
    if (!request || request.owner !== owner) return;
    clearTimeout(request.timer);
    this.pending.delete(response.id);
    if (response.error) {
      const error = Object.assign(new Error(response.error.message), { code: response.error.code, data: response.error.data });
      request.reject(response.error.code === 1010 ? request.timeout ?? (!request.cancelRequested && owner.draining ? owner.draining : error) : error);
    } else request.resolve(response.result);
  }
}

export function routeRpcCall(
  router: RpcRouter,
  localAgent: { request(method: string, params: unknown): Promise<unknown> },
  method: string,
  params: unknown,
): Promise<unknown> {
  const repoId = extractRepoId(params);
  return repoId && parseRepoId(repoId) ? router.route(repoId, method, params) : localAgent.request(method, params);
}

function extractRepoId(params: unknown): string | null {
  if (!params || typeof params !== "object") return null;
  if ("repo_id" in params) return typeof params.repo_id === "string" ? params.repo_id : null;
  return Array.isArray(params) && typeof params[0] === "string" && params[0].startsWith("ssh://") ? params[0] : null;
}
