/**
 * RPC Router — routes agent RPC calls to either the local agent or
 * the appropriate remote agent based on repo_id prefix.
 *
 * Remote repos use `ssh://<profileId>/<path>` as their repo_id. The
 * router parses this prefix to determine the target SSH connection
 * and forwards the JSON-RPC 2.0 request over the agent channel.
 */

import { EventEmitter } from "node:events";
import { StringDecoder } from "node:string_decoder";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteCache } from "./ssh-cache";

// ── Types ────────────────────────────────────────────────────────

export interface RepoTransport {
  type: "local" | "remote";
  profileId?: string;
}

export interface RpcRouterOptions {
  /** Per-request timeout in milliseconds (default: 30_000). */
  requestTimeoutMs?: number;
  /** Maximum queued calls during reconnection (default: 50). */
  maxQueuedCalls?: number;
}

interface PendingRequest {
  accept: (response: JsonRpcResponse) => void;
  fail: (reason: Error) => void;
}

interface ChannelState {
  channel: ClientChannel;
  decoder: StringDecoder;
  buffer: string;
  pending: Map<number, PendingRequest>;
  stop: (reason: Error) => void;
}

interface QueuedCall {
  method: string;
  params: unknown;
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}

interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: number;
  method: string;
  params: unknown;
}

interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number | null;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

interface JsonRpcNotification {
  jsonrpc: "2.0";
  method: string;
  params: unknown;
}

type JsonRpcFrame = JsonRpcResponse | JsonRpcNotification;

/** Read-only methods eligible for stale cache fallback during reconnection. */
const CACHEABLE_READ_METHODS = new Set([
  "git_status_porcelain",
  "git_log",
  "git_diff_file",
  "list_refs",
  "repo_summary",
  "hydration_status",
]);

// Only inspected read handlers may be deferred. A denylist would silently
// queue new mutations and execute them after the user lost the connection.
const REPLAYABLE_READ_METHODS = new Set([...CACHEABLE_READ_METHODS, "task_compute_diff", "git_stash_list", "git_blame"]);

// ── Helpers ──────────────────────────────────────────────────────

/**
 * Parse a repo ID to determine if it targets a remote agent.
 * Remote repos use the format: `ssh://<profileId>/<path>`
 *
 * Returns `{ profileId, remotePath }` for remote repos, or `null`
 * for local repos.
 */
export function parseRepoId(repoId: string): { profileId: string; remotePath: string } | null {
  if (!repoId.startsWith("ssh://")) return null;

  const withoutScheme = repoId.slice("ssh://".length);
  const slashIdx = withoutScheme.indexOf("/");
  if (slashIdx < 0) {
    // ssh://<profileId> with no path — treat the path as "/"
    return { profileId: withoutScheme, remotePath: "/" };
  }

  const profileId = withoutScheme.slice(0, slashIdx);
  const remotePath = withoutScheme.slice(slashIdx);
  return { profileId, remotePath };
}

/**
 * Determine whether a method is a read-only operation eligible for
 * stale cache fallback during reconnection.
 */
export function isCacheableReadMethod(method: string): boolean {
  return CACHEABLE_READ_METHODS.has(method);
}

function isNotification(frame: JsonRpcFrame): frame is JsonRpcNotification {
  return "method" in frame && !("id" in frame && frame.id !== null && frame.id !== undefined);
}

// ── RpcRouter ────────────────────────────────────────────────────

export interface BandwidthStats {
  bytesIn: number;
  bytesOut: number;
}

export class RpcRouter extends EventEmitter {
  private transports = new Map<string, RepoTransport>();
  private channels = new Map<string, ChannelState>();
  private queues = new Map<string, QueuedCall[]>();
  private nextId = 1;
  /** Cumulative bandwidth counters per profile (bytes in/out since connection). */
  private bandwidthCounters = new Map<string, BandwidthStats>();

  private readonly requestTimeoutMs: number;
  private readonly maxQueuedCalls: number;
  private readonly sessionManager: SshSessionManager;
  private readonly localAgent: { request(method: string, params: unknown): Promise<unknown> };
  private remoteCache: RemoteCache | null = null;

  constructor(
    sessionManager: SshSessionManager,
    localAgent: { request(method: string, params: unknown): Promise<unknown> },
    options: RpcRouterOptions = {},
  ) {
    super();
    this.sessionManager = sessionManager;
    this.localAgent = localAgent;
    this.requestTimeoutMs = options.requestTimeoutMs ?? 30_000;
    this.maxQueuedCalls = options.maxQueuedCalls ?? 50;
  }

  /**
   * Set the remote cache instance for stale data fallback during
   * reconnection. When set, read-only operations return cached data
   * with `stale: true` instead of being queued.
   */
  setCache(cache: RemoteCache): void {
    this.remoteCache = cache;
  }

  /**
   * Register a repo ID with its transport type. For remote repos,
   * this is called when the user opens a remote repository.
   */
  registerTransport(repoId: string, transport: RepoTransport): void {
    this.transports.set(repoId, transport);
  }

  /**
   * Remove a registered transport (e.g., when a repo is closed).
   */
  unregisterTransport(repoId: string): void {
    this.transports.delete(repoId);
  }

  /**
   * Route an RPC call to the appropriate agent (local or remote).
   * Determines the target based on the repo_id prefix or registered
   * transport.
   */
  async route(repoId: string, method: string, params: unknown): Promise<unknown> {
    // First, check if the repo_id has an ssh:// prefix
    const parsed = parseRepoId(repoId);
    if (parsed) {
      return this.sendToRemoteAgent(parsed.profileId, method, params);
    }

    // Check registered transports
    const transport = this.transports.get(repoId);
    if (transport && transport.type === "remote" && transport.profileId) {
      return this.sendToRemoteAgent(transport.profileId, method, params);
    }

    // Default: local agent
    return this.localAgent.request(method, params);
  }

  /** Runs independent calls through the same routing and ownership as single calls. */
  async batchRpc(
    repoId: string,
    calls: Array<{ method: string; params: unknown }>,
  ): Promise<Array<{ result?: unknown; error?: { code: number; message: string; data?: unknown } }>> {
    // The sidecar accepts one request object per NDJSON line, not JSON arrays.
    // Reusing route also preserves registered transports and reconnect admission.
    return Promise.all(calls.map(async call => {
      try {
        return { result: await this.route(repoId, call.method, call.params) };
      } catch (reason: unknown) {
        const error = reason instanceof Error ? reason : new Error(String(reason));
        const details = error as Error & { code?: unknown; data?: unknown };
        const code = typeof details.code === "number" ? details.code : -32000;
        return { error: { code, message: error.message, ...(details.data !== undefined ? { data: details.data } : {}) } };
      }
    }));
  }

  /**
   * Attach to a remote agent channel to listen for notifications.
   * Call this after the agent channel is established on a connection.
   */
  attachChannel(profileId: string, channel: ClientChannel): void {
    const previous = this.channels.get(profileId);
    if (previous?.channel === channel) return;
    previous?.stop(new Error("Remote agent channel replaced before its RPC completed; outcome is unknown"));
    if (channel.destroyed || channel.readableEnded || channel.writableEnded) throw new Error("Remote agent channel is already closed");
    const owner: ChannelState = {
      channel, decoder: new StringDecoder("utf8"), buffer: "", pending: new Map(),
      stop: reason => {
        if (this.channels.get(profileId) !== owner) return;
        this.channels.delete(profileId);
        channel.removeListener("data", data);
        channel.removeListener("end", end);
        owner.buffer = "";
        for (const pending of owner.pending.values()) pending.fail(reason);
        owner.pending.clear();
      },
    };
    const data = (chunk: Buffer) => {
      if (this.channels.get(profileId) !== owner) return;
      const counter = this.bandwidthCounters.get(profileId);
      if (counter) counter.bytesIn += chunk.length;
      this.handleChannelData(profileId, owner, chunk);
    };
    const end = () => owner.stop(new Error("Remote agent output ended before its RPC completed; outcome is unknown"));
    const error = (reason: Error) => owner.stop(reason);
    const close = () => {
      owner.stop(new Error("Remote agent channel closed before its RPC completed; outcome is unknown"));
      // The session manager owns the physical stream. Keep its error listener
      // until close, so a retired channel cannot emit an unhandled late error.
      channel.removeListener("error", error);
      channel.removeListener("close", close);
    };
    this.channels.set(profileId, owner);
    if (!this.bandwidthCounters.has(profileId)) this.bandwidthCounters.set(profileId, { bytesIn: 0, bytesOut: 0 });
    channel.on("data", data);
    channel.on("end", end);
    channel.on("error", error);
    channel.on("close", close);
  }

  /** Retires sent calls without moving the unsent read queue to another channel. */
  detachChannel(profileId: string): void {
    this.channels.get(profileId)?.stop(new Error("Remote agent disconnected before its RPC completed; outcome is unknown"));
  }

  /**
   * Replay queued calls after a successful reconnection.
   * Called by the session manager when a connection is re-established.
   */
  async replayQueue(profileId: string): Promise<void> {
    const queue = this.queues.get(profileId);
    if (!queue || queue.length === 0) return;

    // Take ownership of the queue and clear it
    const calls = [...queue];
    this.queues.delete(profileId);

    for (const call of calls) {
      try {
        const result = await this.sendToRemoteAgent(profileId, call.method, call.params);
        call.resolve(result);
      } catch (err) {
        call.reject(err instanceof Error ? err : new Error(String(err)));
      }
    }
  }

  /**
   * Clear the queue for a profile (e.g., on permanent disconnect).
   */
  clearQueue(profileId: string): void {
    const queue = this.queues.get(profileId);
    if (!queue) return;

    for (const call of queue) {
      call.reject(new Error("Connection lost permanently"));
    }
    this.queues.delete(profileId);
  }

  /**
   * Get cumulative bandwidth stats for a profile since connection.
   */
  getBandwidth(profileId: string): BandwidthStats {
    return this.bandwidthCounters.get(profileId) ?? { bytesIn: 0, bytesOut: 0 };
  }

  /**
   * Reset bandwidth counters for a profile (e.g., on new connection).
   */
  resetBandwidth(profileId: string): void {
    this.bandwidthCounters.set(profileId, { bytesIn: 0, bytesOut: 0 });
  }

  /** Sends a profile RPC through connection admission before writing to its channel. */
  async sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    const status = this.sessionManager.getStatus(profileId);

    if (status === "reconnecting") {
      if (!REPLAYABLE_READ_METHODS.has(method)) {
        const err = new Error("Connection lost: mutating or unclassified operations are unavailable during reconnection");
        (err as Error & { code?: string }).code = "SSH_RECONNECTING";
        throw err;
      }

      // For cacheable read-only operations, return stale cached data if available
      if (isCacheableReadMethod(method) && this.remoteCache) {
        const repoId = this.extractRepoIdFromParams(params);
        if (repoId) {
          const cacheKey = `${repoId}:${method}`;
          const cached = this.remoteCache.getCached(cacheKey);
          if (cached) {
            return { ...this.wrapResult(cached.data), __stale: true };
          }
        }
      }

      // Queue non-cacheable read-only calls for replay
      return this.enqueueCall(profileId, method, params);
    }

    if (status !== "connected" && !(status === "deploying" && method === "capabilities")) {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    return this.writeRequest(profileId, method, params);
  }

  /**
   * Wrap a cached result for return. If the data is already an object,
   * return it directly (the __stale flag is added by the caller). If
   * it's a primitive, wrap it in an object.
   */
  private wrapResult(data: unknown): Record<string, unknown> {
    if (data !== null && typeof data === "object" && !Array.isArray(data)) {
      return data as Record<string, unknown>;
    }
    return { data };
  }

  /**
   * Extract repo_id from RPC params for cache key construction.
   */
  private extractRepoIdFromParams(params: unknown): string | null {
    if (params && typeof params === "object" && "repo_id" in params) {
      const id = (params as Record<string, unknown>).repo_id;
      return typeof id === "string" ? id : null;
    }
    return null;
  }

  /**
   * Serialize a JSON-RPC 2.0 request, write to the remote agent's SSH
   * channel stdin, and wait for the correlated response from stdout.
   * Uses NDJSON framing (newline-delimited JSON).
   *
   * The per-request timeout is determined by the profile's
   * `operationTimeoutMs` setting, falling back to the router-level
   * `requestTimeoutMs` default.
   */
  private async writeRequest(profileId: string, method: string, params: unknown): Promise<unknown> {
    const owner = this.channels.get(profileId);
    if (!owner) throw new Error(`No active agent channel for profile: ${profileId}`);
    const timeoutMs = this.sessionManager.getConnection(profileId)?.profile?.operationTimeoutMs ?? this.requestTimeoutMs;
    const request: JsonRpcRequest = { jsonrpc: "2.0", id: this.nextId++, method, params };
    // Serialize before registering callbacks. A circular input must not leave
    // a pending entry or timer after rejecting without writing any bytes.
    const frame = JSON.stringify(request) + "\n";
    return new Promise((resolve, reject) => {
      let settled = false;
      const cleanup = () => { clearTimeout(timer); owner.pending.delete(request.id); };
      const fail = (reason: Error) => {
        if (settled) return;
        settled = true; cleanup(); reject(reason);
      };
      const timer = setTimeout(() => fail(new Error(`RPC timeout: ${method} did not respond within ${timeoutMs}ms`)), timeoutMs);
      const accept = (response: JsonRpcResponse) => {
        if (settled) return;
        if (response.error) {
          fail(Object.assign(new Error(response.error.message), { code: response.error.code, data: response.error.data }));
          return;
        }
        settled = true; cleanup(); resolve(response.result);
      };
      owner.pending.set(request.id, { accept, fail });
      const counter = this.bandwidthCounters.get(profileId);
      if (counter) counter.bytesOut += Buffer.byteLength(frame, "utf8");
      try { owner.channel.write(frame, (error?: Error | null) => { if (error) fail(error); }); }
      catch (error: unknown) { fail(error instanceof Error ? error : new Error(String(error))); }
    });
  }

  /**
   * Queue a read-only call during reconnection. Rejects if the queue
   * is full (drops oldest to make room).
   */
  private enqueueCall(profileId: string, method: string, params: unknown): Promise<unknown> {
    let queue = this.queues.get(profileId);
    if (!queue) {
      queue = [];
      this.queues.set(profileId, queue);
    }

    // Drop oldest if at capacity
    if (queue.length >= this.maxQueuedCalls) {
      const dropped = queue.shift();
      if (dropped) {
        dropped.reject(new Error("Call dropped: reconnection queue overflow"));
      }
    }

    return new Promise<unknown>((resolve, reject) => {
      queue!.push({ method, params, resolve, reject });
    });
  }

  /**
   * Handle incoming data from a remote agent channel. Implements
   * NDJSON parsing: buffer partial lines, process complete lines.
   */
  private handleChannelData(profileId: string, owner: ChannelState, data: Buffer): void {
    const lines = (owner.buffer + owner.decoder.write(data)).split("\n");
    owner.buffer = lines.pop() ?? "";
    for (const line of lines) {
      if (line.trim()) this.processLine(profileId, owner, line);
    }
  }

  /**
   * Process a single NDJSON line from the remote agent. Could be a
   * response (has `id`) or a notification (has `method`, no `id`).
   */
  private processLine(profileId: string, owner: ChannelState, line: string): void {
    let frame: JsonRpcFrame;
    try {
      frame = JSON.parse(line) as JsonRpcFrame;
    } catch {
      // Malformed JSON — skip
      return;
    }

    if (frame === null || typeof frame !== "object") return;

    // Notification: no `id` field, has `method`
    if (isNotification(frame)) {
      this.emit("notification", {
        profileId,
        method: (frame as JsonRpcNotification).method,
        params: (frame as JsonRpcNotification).params,
      });
      return;
    }

    // Response: has `id`
    this.handleSingleResponse(owner, frame as JsonRpcResponse);
  }

  private handleSingleResponse(owner: ChannelState, response: JsonRpcResponse): void {
    if (!response || typeof response.id !== "number") return;
    const pending = owner.pending.get(response.id);
    if (!pending) return;
    pending.accept(response);
  }
}

// ── Integration helper for main.ts ──────────────────────────────

/**
 * Intercept an RPC call and route it through the RPC router if the
 * repo_id indicates a remote repo. Returns the result from either
 * the local or remote agent.
 *
 * Usage in main.ts:
 * ```ts
 * ipcMain.handle("crab:rpc", async (_evt, method, params) => {
 *   return routeRpcCall(router, agent, method, params);
 * });
 * ```
 */
export function routeRpcCall(
  router: RpcRouter,
  localAgent: { request(method: string, params: unknown): Promise<unknown> },
  method: string,
  params: unknown,
): Promise<unknown> {
  // Extract repo_id from params if present
  const repoId = extractRepoId(params);

  if (repoId && parseRepoId(repoId)) {
    return router.route(repoId, method, params);
  }

  // No remote repo_id — send to local agent
  return localAgent.request(method, params);
}

/**
 * Extract the repo_id field from RPC params. Params can be an object
 * with a `repo_id` field, or the first element of an array.
 */
function extractRepoId(params: unknown): string | null {
  if (params && typeof params === "object") {
    if ("repo_id" in params) {
      const id = (params as Record<string, unknown>).repo_id;
      return typeof id === "string" ? id : null;
    }
    // Array params: check first element
    if (Array.isArray(params) && params.length > 0 && typeof params[0] === "string") {
      return params[0].startsWith("ssh://") ? params[0] : null;
    }
  }
  return null;
}
