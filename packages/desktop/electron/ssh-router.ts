/**
 * RPC Router — routes agent RPC calls to either the local agent or
 * the appropriate remote agent based on repo_id prefix.
 *
 * Remote repos use `ssh://<profileId>/<path>` as their repo_id. The
 * router parses this prefix to determine the target SSH connection
 * and forwards the JSON-RPC 2.0 request over the agent channel.
 */

import { EventEmitter } from "node:events";
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
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
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

/** Methods that mutate state and must NOT be queued during reconnection. */
const WRITE_METHODS = new Set([
  "cache_clean",
  "add",
  "git_commit",
  "git_stage",
  "git_checkout",
  "git_push",
  "git_pull",
  "git_stash_save",
  "git_branch_create",
  "git_branch_delete",
  "hydrate",
  "dehydrate",
  "op_start",
  "op_continue",
  "push",
  "worktree_create",
  "worktree_destroy",
]);

/** Read-only methods eligible for stale cache fallback during reconnection. */
const CACHEABLE_READ_METHODS = new Set([
  "git_status_porcelain",
  "git_log",
  "git_diff_file",
  "list_refs",
  "repo_summary",
  "hydration_status",
]);

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
 * Determine whether a method is a write operation that should be
 * rejected immediately during reconnection.
 */
export function isWriteMethod(method: string): boolean {
  for (const prefix of WRITE_METHODS) {
    if (method === prefix || method.startsWith(prefix)) {
      return true;
    }
  }
  return false;
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
  private pending = new Map<number, PendingRequest>();
  private queues = new Map<string, QueuedCall[]>();
  private nextId = 1;
  private lineBuffers = new Map<string, string>();
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
      return this.routeToRemote(parsed.profileId, method, params);
    }

    // Check registered transports
    const transport = this.transports.get(repoId);
    if (transport && transport.type === "remote" && transport.profileId) {
      return this.routeToRemote(transport.profileId, method, params);
    }

    // Default: local agent
    return this.localAgent.request(method, params);
  }

  /**
   * Send multiple independent RPC calls as a JSON-RPC batch array.
   * Returns an array of responses in the same order as the input calls.
   * Reduces round-trips for Dashboard/status views.
   */
  async batchRpc(
    repoId: string,
    calls: Array<{ method: string; params: unknown }>,
  ): Promise<Array<{ result?: unknown; error?: { code: number; message: string; data?: unknown } }>> {
    const parsed = parseRepoId(repoId);
    if (!parsed) {
      // Local agent: execute sequentially (local agent doesn't support batch)
      const results: Array<{ result?: unknown; error?: { code: number; message: string; data?: unknown } }> = [];
      for (const call of calls) {
        try {
          const result = await this.localAgent.request(call.method, call.params);
          results.push({ result });
        } catch (err) {
          const message = err instanceof Error ? err.message : String(err);
          results.push({ error: { code: -32000, message } });
        }
      }
      return results;
    }

    return this.sendBatchToRemote(parsed.profileId, calls);
  }

  /**
   * Attach to a remote agent channel to listen for notifications.
   * Call this after the agent channel is established on a connection.
   */
  attachChannel(profileId: string, channel: ClientChannel): void {
    this.lineBuffers.set(profileId, "");
    // Initialize bandwidth counter for this profile
    if (!this.bandwidthCounters.has(profileId)) {
      this.bandwidthCounters.set(profileId, { bytesIn: 0, bytesOut: 0 });
    }

    channel.on("data", (data: Buffer) => {
      // Track incoming bytes
      const counter = this.bandwidthCounters.get(profileId);
      if (counter) {
        counter.bytesIn += data.length;
      }
      this.handleChannelData(profileId, data);
    });

    channel.on("close", () => {
      this.lineBuffers.delete(profileId);
      // Reject all pending requests for this profile
      for (const [] of this.pending) {
        // We can't easily filter by profileId here, so we leave them
        // to time out. The timeout mechanism handles cleanup.
      }
    });
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

  // ── Private ────────────────────────────────────────────────────

  private async routeToRemote(profileId: string, method: string, params: unknown): Promise<unknown> {
    const status = this.sessionManager.getStatus(profileId);

    if (status === "reconnecting") {
      // Write operations are rejected immediately during reconnection
      if (isWriteMethod(method)) {
        const err = new Error("Connection lost: write operations are unavailable during reconnection");
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

    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    return this.sendToRemoteAgent(profileId, method, params);
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
  sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn || !conn.channels.agent) {
      return Promise.reject(new Error(`No agent channel for profile: ${profileId}`));
    }

    const channel = conn.channels.agent;
    const id = this.nextId++;
    const timeoutMs = conn.profile?.operationTimeoutMs ?? this.requestTimeoutMs;

    const request: JsonRpcRequest = {
      jsonrpc: "2.0",
      id,
      method,
      params,
    };

    return new Promise<unknown>((resolve, reject) => {
      // Set up per-request timeout
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`RPC timeout: ${method} did not respond within ${timeoutMs}ms`));
      }, timeoutMs);

      this.pending.set(id, { resolve, reject, timer });

      // Write NDJSON frame (JSON + newline)
      const frame = JSON.stringify(request) + "\n";
      // Track outgoing bytes
      const counter = this.bandwidthCounters.get(profileId);
      if (counter) {
        counter.bytesOut += Buffer.byteLength(frame, "utf8");
      }
      channel.write(frame, (err?: Error | null) => {
        if (err) {
          clearTimeout(timer);
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }

  /**
   * Send a batch of RPC calls as a JSON-RPC batch array to the remote agent.
   */
  private sendBatchToRemote(
    profileId: string,
    calls: Array<{ method: string; params: unknown }>,
  ): Promise<Array<{ result?: unknown; error?: { code: number; message: string; data?: unknown } }>> {
    const conn = this.sessionManager.getConnection(profileId);
    if (!conn || !conn.channels.agent) {
      return Promise.reject(new Error(`No agent channel for profile: ${profileId}`));
    }

    const channel = conn.channels.agent;

    // Assign IDs to each call in the batch
    const batchRequests: JsonRpcRequest[] = calls.map((call) => ({
      jsonrpc: "2.0" as const,
      id: this.nextId++,
      method: call.method,
      params: call.params,
    }));

    const ids = batchRequests.map((r) => r.id);

    return new Promise<Array<{ result?: unknown; error?: { code: number; message: string; data?: unknown } }>>((resolve, reject) => {
      const results = new Map<number, { result?: unknown; error?: { code: number; message: string; data?: unknown } }>();
      let settled = false;

      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        // Clean up pending entries
        for (const id of ids) {
          this.pending.delete(id);
        }
        reject(new Error(`Batch RPC timeout: did not receive all responses within ${this.requestTimeoutMs}ms`));
      }, this.requestTimeoutMs);

      const checkComplete = () => {
        if (results.size === ids.length && !settled) {
          settled = true;
          clearTimeout(timer);
          // Return results in the same order as input calls
          const ordered = ids.map((id) => results.get(id) ?? { error: { code: -32000, message: "Missing response" } });
          resolve(ordered);
        }
      };

      // Register pending entries for each request in the batch
      for (const req of batchRequests) {
        const batchTimer = setTimeout(() => {
          // Individual timeout — handled by the batch-level timer
        }, this.requestTimeoutMs);

        this.pending.set(req.id, {
          resolve: (value: unknown) => {
            clearTimeout(batchTimer);
            results.set(req.id, { result: value });
            checkComplete();
          },
          reject: (err: Error) => {
            clearTimeout(batchTimer);
            results.set(req.id, { error: { code: -32000, message: err.message } });
            checkComplete();
          },
          timer: batchTimer,
        });
      }

      // Write the batch as a single NDJSON line (JSON array + newline)
      const frame = JSON.stringify(batchRequests) + "\n";
      // Track outgoing bytes for batch
      const counter = this.bandwidthCounters.get(profileId);
      if (counter) {
        counter.bytesOut += Buffer.byteLength(frame, "utf8");
      }
      channel.write(frame, (err?: Error | null) => {
        if (err && !settled) {
          settled = true;
          clearTimeout(timer);
          for (const id of ids) {
            this.pending.delete(id);
          }
          reject(err);
        }
      });
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
  private handleChannelData(profileId: string, data: Buffer): void {
    const text = data.toString("utf8");
    const buffer = (this.lineBuffers.get(profileId) ?? "") + text;
    const lines = buffer.split("\n");

    // The last element is either empty (if data ended with \n) or a
    // partial line that needs more data.
    this.lineBuffers.set(profileId, lines.pop() ?? "");

    for (const line of lines) {
      if (!line.trim()) continue;
      this.processLine(profileId, line);
    }
  }

  /**
   * Process a single NDJSON line from the remote agent. Could be a
   * response (has `id`) or a notification (has `method`, no `id`).
   */
  private processLine(profileId: string, line: string): void {
    let frame: JsonRpcFrame;
    try {
      frame = JSON.parse(line) as JsonRpcFrame;
    } catch {
      // Malformed JSON — skip
      return;
    }

    // Handle batch responses (array of responses)
    if (Array.isArray(frame)) {
      for (const item of frame as JsonRpcResponse[]) {
        this.handleSingleResponse(item);
      }
      return;
    }

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
    this.handleSingleResponse(frame as JsonRpcResponse);
  }

  private handleSingleResponse(response: JsonRpcResponse): void {
    if (response.id == null) return;

    const id = typeof response.id === "number" ? response.id : Number(response.id);
    const pending = this.pending.get(id);
    if (!pending) return;

    clearTimeout(pending.timer);
    this.pending.delete(id);

    if (response.error) {
      const err = new Error(response.error.message);
      (err as Error & { code?: number; data?: unknown }).code = response.error.code;
      (err as Error & { code?: number; data?: unknown }).data = response.error.data;
      pending.reject(err);
    } else {
      pending.resolve(response.result);
    }
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
