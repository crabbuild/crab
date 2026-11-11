import { parseRepoId, sameRemoteEndpoint, type RemoteEndpoint } from "./ssh-types";
import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { TextDecoder } from "node:util";
import type { ClientChannel } from "ssh2";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteCache } from "./ssh-cache";
import { nativeSessionLost } from "./repo-capabilities";
import type { StorageConnections } from "./storage-connections";

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
  closed: Promise<void>;
  finish: () => void;
  failure?: Error;
  draining?: Error;
}
interface PendingRequest {
  owner: ChannelOwner;
  method: string;
  operation?: { op_id: string; repo_id?: string };
  progress?: { job_id: string; repo_id: string };
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer?: ReturnType<typeof setTimeout>;
  timeout?: Error;
  cancelRequested?: boolean;
}
interface QueuedCall {
  repoId: string;
  method: string;
  params: unknown;
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  stopped: Promise<void>;
  cancel: (error: Error) => void;
  failure?: Error;
}
interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number | null;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}
type BatchReply = { result?: unknown; error?: { code: number; message: string; data?: unknown } };
const MAX_CONCURRENT_CALLS = 4;

// These methods own repository setup or bounded content work. Cancellation
// completes after native cleanup; the metadata timer must not discard its reply.
const OWNED_METHODS = new Set([
  "open_local", "open_url",
  "file_open", "file_diff", "file_chunks", "file_materialize", "files_materialize", "files_dehydrate", "read_range", "read_text", "read_bytes",
  "preview_parquet", "preview_safetensors", "preview_npy", "preview_npz", "preview_zip",
  "preview_notebook_meta", "preview_torch_checkpoint", "preview_sqlite", "file_query",
]);
const CACHEABLE_READ_METHODS = new Set([
  "git_status_porcelain", "git_log", "list_refs", "repo_summary", "hydration_status",
]);
// Worktree discovery may wait for repository recovery, but its paths must
// never be returned from a stale cache as if they were the live checkout.
const REPLAYABLE_READ_METHODS = new Set([...CACHEABLE_READ_METHODS, "git_worktree_list"]);
const PROGRESS_METHODS = new Set([
  "progress/line", "progress/transfer", "progress/complete", "progress/partial",
  "crab/add_progress", "crab/push_step", "crab/push_progress",
]);

export function isCacheableReadMethod(method: string): boolean { return CACHEABLE_READ_METHODS.has(method); }

export interface BandwidthStats { bytesIn: number; bytesOut: number }

export class RpcRouter extends EventEmitter {
  private transports = new Map<string, RepoTransport>();
  private channels = new Map<string, ChannelOwner>();
  private pending = new Map<number, PendingRequest>();
  private queues = new Map<string, Set<QueuedCall>>();
  private replays = new Map<string, { owner: ChannelOwner; work: Promise<void> }>();
  private nextId = 1;
  private bandwidthCounters = new Map<string, BandwidthStats>();
  private remoteCache: RemoteCache | null = null;
  private readonly requestTimeoutMs: number;
  private readonly ownerTimeoutMs?: number;
  private readonly maxQueuedCalls: number;

  constructor(
    private readonly sessionManager: SshSessionManager,
    private readonly localAgent: { request(method: string, params: unknown): Promise<unknown> },
    private readonly storageConnections: Pick<StorageConnections, "request" | "redactValue">,
    options: RpcRouterOptions = {},
  ) {
    super();
    this.requestTimeoutMs = options.requestTimeoutMs ?? 30_000;
    this.ownerTimeoutMs = options.requestTimeoutMs;
    this.maxQueuedCalls = options.maxQueuedCalls ?? 50;
  }

  setCache(cache: RemoteCache): void { this.remoteCache = cache; }
  registerTransport(repoId: string, transport: RepoTransport): void { this.transports.set(repoId, transport); }
  unregisterTransport(repoId: string): void { this.transports.delete(repoId); }

  async route(repoId: string, method: string, params: unknown): Promise<unknown> {
    const parsed = parseRepoId(repoId);
    const transport = this.transports.get(repoId);
    const profileId = parsed?.profileId ?? (transport?.type === "remote" ? transport.profileId : undefined);
    if (profileId) {
      // Process readiness precedes repository replay. Join that repository's
      // admission before metadata dispatch. Captures and cancellation retain
      // their live-session contract rather than acquiring a hidden retry queue.
      if (this.sessionManager.getStatus(profileId) === "connected" && REPLAYABLE_READ_METHODS.has(method)) {
        const pending = this.sessionManager.repositoryWork(repoId);
        if (pending) await pending;
      }
      return this.routeToRemote(profileId, repoId, method, params);
    }
    return this.localAgent.request(method, params);
  }

  /** Independent calls share single-request framing, ownership and errors. */
  async batchRpc(repoId: string, calls: Array<{ method: string; params: unknown }>): Promise<BatchReply[]> {
    const results: BatchReply[] = new Array(calls.length);
    let next = 0;
    // Workspace polling must not spawn a Git child for every repository at once.
    await Promise.all(Array.from({ length: Math.min(MAX_CONCURRENT_CALLS, calls.length) }, async () => {
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
    let finish!: () => void;
    const closed = new Promise<void>(resolve => { finish = resolve; });
    const owner: ChannelOwner = { profileId, channel, parts: [], bytes: 0, closed, finish };
    this.channels.set(profileId, owner);
    if (!this.bandwidthCounters.has(profileId)) this.resetBandwidth(profileId);
    const onData = (data: Buffer) => {
      if (this.channels.get(profileId) !== owner || owner.failure) return;
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
    if (owner.failure) return;
    owner.failure = error;
    owner.parts = []; owner.bytes = 0;
    if (this.channels.get(owner.profileId) === owner) this.channels.delete(owner.profileId);
    for (const [id, request] of this.pending) {
      if (request.owner !== owner) continue;
      clearTimeout(request.timer);
      this.pending.delete(id);
      request.reject(error);
    }
    this.emit("remote-channel-retired", { profileId: owner.profileId, owner, reason: error });
    owner.finish();
    // EOF lets the native agent cancel and join reads, then close its SDK/DBs.
    owner.channel.end();
  }

  /** Retires sent calls without moving the unsent read queue to another channel. */
  detachChannel(profileId: string): void {
    const owner = this.channels.get(profileId);
    if (owner) this.failChannel(owner, new Error("Remote agent disconnected before its RPC completed; outcome is unknown"));
  }

  /** Captures the channel identity for a native process's entire lifetime. */
  remoteChannelOwner(profileId: string): object | undefined { return this.channels.get(profileId); }

  /** Captures the verified destination of the currently attached agent. */
  remoteEndpoint(profileId: string): RemoteEndpoint {
    const connection = this.sessionManager.getConnection(profileId);
    const channel = this.channels.get(profileId)?.channel;
    if (!channel || connection?.channels.agent !== channel || !connection.endpoint) {
      throw new Error("Remote endpoint requires an attached, authenticated SSH agent");
    }
    return { ...connection.endpoint };
  }

  /** Refuses to infer a durable task's destination from an editable profile. */
  assertRemoteEndpoint(profileId: string, expected: RemoteEndpoint | undefined): void {
    if (!expected) throw new Error("Remote task endpoint was not recorded. Recover its host binding explicitly before continuing.");
    const current = this.remoteEndpoint(profileId);
    if (!sameRemoteEndpoint(current, expected)) {
      throw new Error("SSH profile endpoint changed. Restore the task's original host, account and host key before continuing.");
    }
  }

  /** Sends only on the captured channel; a replacement cannot inherit process writes. */
  requestOnRemoteChannel(profileId: string, owner: object, method: string, params: unknown): Promise<unknown> {
    if (this.channels.get(profileId) !== owner) return Promise.reject(new Error("Native process transport was replaced; remote closure is unconfirmed"));
    return this.sendPreparedToRemoteAgent(this.channels.get(profileId)!, method, params);
  }

  replayQueue(profileId: string): Promise<void> {
    const active = this.replays.get(profileId);
    if (active) return this.channels.get(profileId) === active.owner ? active.work : active.work.then(() => this.replayQueue(profileId));
    const queue = this.queues.get(profileId);
    if (!queue?.size) return Promise.resolve();
    const owner = this.channels.get(profileId);
    if (!owner) {
      const error = new Error(`No agent channel for profile: ${profileId}`);
      for (const call of queue) call.cancel(error);
      queue.clear(); this.queues.delete(profileId);
      return Promise.resolve();
    }
    const entry = { owner, work: Promise.resolve().then(async () => {
      const running = new Set<Promise<void>>();
      while (queue.size && !owner.failure) {
        const repositories = new Map<string, QueuedCall[]>();
        for (const call of queue) {
          const calls = repositories.get(call.repoId) ?? [];
          calls.push(call); repositories.set(call.repoId, calls);
        }
        await Promise.all([...repositories.values()].map(async calls => {
          for (const call of calls) {
            try {
              const opening = this.sessionManager.repositoryWork(call.repoId);
              if (opening) await Promise.race([opening, call.stopped, owner.closed]);
              while (!call.failure && !owner.failure && running.size >= MAX_CONCURRENT_CALLS) {
                await Promise.race([...running, call.stopped, owner.closed]);
              }
              if (call.failure) throw call.failure;
              if (owner.failure) throw owner.failure;
              // Keep admission in the same queue through authentication. A
              // cancelled read must never be sent when credentials arrive late.
              const operation = (async () => {
                const prepared = this.storageConnections.request(call.method, call.params, (method, params) => {
                  if (call.failure) return Promise.reject(call.failure);
                  queue.delete(call);
                  return this.sendPreparedToRemoteAgent(owner, method, params);
                }, profileId);
                const result = await Promise.race([prepared, call.stopped, owner.closed]);
                if (call.failure) throw call.failure;
                if (owner.failure) throw owner.failure;
                call.resolve(result);
              })();
              const settled = operation.then(() => {}, () => {});
              running.add(settled);
              try { await operation; } finally { running.delete(settled); }
            } catch (error) { call.reject(error instanceof Error ? error : new Error(String(error))); }
            finally { queue.delete(call); }
          }
        }));
      }
    }).finally(() => {
      if (!queue.size && this.queues.get(profileId) === queue) this.queues.delete(profileId);
      if (this.replays.get(profileId) === entry) this.replays.delete(profileId);
    }) };
    this.replays.set(profileId, entry);
    return entry.work;
  }
  clearQueue(profileId: string): void {
    for (const call of this.queues.get(profileId) ?? []) call.cancel(new Error("Connection lost permanently"));
    this.queues.get(profileId)?.clear();
    this.queues.delete(profileId);
  }
  getBandwidth(profileId: string): BandwidthStats { return this.bandwidthCounters.get(profileId) ?? { bytesIn: 0, bytesOut: 0 }; }
  resetBandwidth(profileId: string): void { this.bandwidthCounters.set(profileId, { bytesIn: 0, bytesOut: 0 }); }

  private async routeToRemote(profileId: string, repoId: string, method: string, params: unknown): Promise<unknown> {
    const status = this.sessionManager.getStatus(profileId);
    if (method === "rpc/cancel" && params && typeof params === "object" && "op_id" in params) {
      const queue = this.queues.get(profileId);
      const call = queue && [...queue].find(call => call.repoId === repoId && call.params && typeof call.params === "object" && "op_id" in call.params && call.params.op_id === params.op_id);
      if (queue && call) {
        queue.delete(call);
        call.cancel(Object.assign(new Error("Queued operation cancelled"), { code: 1010, data: { kind: "op_cancelled" } }));
        return { ok: true };
      }
    }
    if (status === "reconnecting") {
      // Only explicitly cacheable metadata can be replayed. Captured sources,
      // cancellation signals and unknown mutations belong to one agent session.
      if (!REPLAYABLE_READ_METHODS.has(method)) throw Object.assign(new Error("Connection lost: live operations are unavailable during reconnection"), { code: "SSH_RECONNECTING" });
      const cached = isCacheableReadMethod(method) && repoId ? this.remoteCache?.getCached(`${repoId}:${method}`) : null;
      if (cached) {
        const data = cached.data;
        return { ...(data !== null && typeof data === "object" && !Array.isArray(data) ? data : { data }), __stale: true };
      }
      let queue = this.queues.get(profileId);
      if (!queue) { queue = new Set(); this.queues.set(profileId, queue); }
      if (queue.size >= this.maxQueuedCalls) {
        const oldest = queue.values().next().value;
        if (oldest) { queue.delete(oldest); oldest.cancel(new Error("Call dropped: reconnection queue overflow")); }
      }
      return new Promise((resolve, reject) => {
        let stop!: () => void;
        const stopped = new Promise<void>(done => { stop = done; });
        const call: QueuedCall = { repoId, method, params, resolve, reject, stopped, cancel: error => {
          call.failure = error; stop(); reject(error);
        } };
        queue.add(call);
      });
    }
    if (status !== "connected") throw new Error(`SSH connection not available (status: ${status})`);
    return this.sendToRemoteAgent(profileId, method, params);
  }

  async sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
    const status = this.sessionManager.getStatus(profileId);
    if (status === "reconnecting") return this.routeToRemote(profileId, extractRepoId(params) ?? "", method, params);
    if (status !== "connected" && !(status === "deploying" && method === "capabilities")) throw new Error(`SSH connection not available (status: ${status})`);
    const owner = this.channelOwner(profileId);
    return this.storageConnections.request(method, params, (name, payload) => this.sendPreparedToRemoteAgent(owner, name, payload), profileId);
  }

  /** Drain a native owner without forgetting its desired credential binding. */
  async retireRepository(profileId: string, repoId: string): Promise<unknown> {
    return this.sendPreparedToRemoteAgent(this.channelOwner(profileId), "close", { repo_id: repoId });
  }

  private channelOwner(profileId: string): ChannelOwner {
    const connection = this.sessionManager.getConnection(profileId);
    const owner = this.channels.get(profileId);
    if (!owner || owner.channel !== connection?.channels.agent) throw new Error(`No agent channel for profile: ${profileId}`);
    return owner;
  }

  private sendPreparedToRemoteAgent(owner: ChannelOwner, method: string, params: unknown): Promise<unknown> {
    if (owner.failure) return Promise.reject(owner.failure);
    const profileId = owner.profileId;
    const connection = this.sessionManager.getConnection(profileId);
    if (this.channels.get(profileId) !== owner || owner.channel !== connection?.channels.agent) return Promise.reject(new Error(`No agent channel for profile: ${profileId}`));
    const owned = OWNED_METHODS.has(method);
    // Release must finish even if the initiating view/profile deadline expires.
    const timeout = method === "file_close" || method === "close" ? undefined
      : connection.profile?.operationTimeoutMs ?? (owned ? this.ownerTimeoutMs : this.requestTimeoutMs);
    let fields = params !== null && typeof params === "object" && !Array.isArray(params) ? params as Record<string, unknown> : null;
    if (fields && Object.hasOwn(fields, "native_session_id")) {
      const { native_session_id: expected, ...nativeParams } = fields;
      if (!expected || this.sessionManager.repositoryRuntime(profileId, String(fields.repo_id)).sessionId !== expected) {
        if (method === "file_close") return Promise.resolve({ closed: fields.file_id });
        return Promise.reject(nativeSessionLost());
      }
      params = nativeParams; fields = nativeParams;
    }
    const enriched = owned && fields && typeof fields.op_id !== "string" ? { ...fields, op_id: randomUUID() } : params;
    return this.send(owner, method, enriched, timeout);
  }

  private send(owner: ChannelOwner, method: string, params: unknown, timeoutMs: number | undefined): Promise<unknown> {
    const failure = owner.failure ?? owner.draining;
    if (failure) return Promise.reject(failure);
    const fields = params !== null && typeof params === "object" && !Array.isArray(params) ? params as Record<string, unknown> : null;
    const operation = method !== "rpc/cancel" && typeof fields?.op_id === "string"
      ? { op_id: fields.op_id, ...(typeof fields.repo_id === "string" ? { repo_id: fields.repo_id } : {}) } : undefined;
    const jobId = fields?.op_id ?? fields?.job_id;
    const progress = method !== "rpc/cancel" && typeof jobId === "string" && typeof fields?.repo_id === "string" && parseRepoId(fields.repo_id)?.profileId === owner.profileId
      ? { job_id: jobId, repo_id: fields.repo_id } : undefined;
    if (method === "rpc/cancel") {
      for (const request of this.pending.values()) if (request.owner === owner && request.operation?.op_id === fields?.op_id) request.cancelRequested = true;
    }
    const id = this.nextId++;
    const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      const request: PendingRequest = { owner, method, operation, progress, resolve, reject };
      this.pending.set(id, request);
      if (timeoutMs !== undefined) request.timer = setTimeout(() => {
        const error = Object.assign(new Error(`RPC timeout: ${method} did not respond within ${timeoutMs}ms`), { code: "SSH_RPC_TIMEOUT" });
        if (!operation) { this.pending.delete(id); reject(error); return; }
        request.timeout = error;
        // Signalling is not completion. Keep the original request until its
        // native terminal reply, preserving integrity errors and raced success.
        void this.send(owner, "rpc/cancel", operation, timeoutMs).catch(error => {
          if (owner.failure || owner.draining) return;
          owner.draining = error instanceof Error ? error : new Error(String(error));
          for (const pending of this.pending.values()) if (pending.owner === owner) clearTimeout(pending.timer);
          owner.channel.end();
        });
      }, timeoutMs);
      const counter = this.bandwidthCounters.get(owner.profileId);
      if (counter) counter.bytesOut += Buffer.byteLength(frame);
      try {
        owner.channel.write(frame, error => { if (error) this.failChannel(owner, error); });
      } catch (reason: unknown) {
        this.failChannel(owner, reason instanceof Error ? reason : new Error("SSH frame write failed", { cause: reason }));
      }
    });
  }

  private handleChannelData(owner: ChannelOwner, data: Buffer): void {
    let start = 0;
    while (start < data.length && !owner.failure) {
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
      const notification = this.storageConnections.redactValue("params" in frame ? frame.params : undefined);
      if (PROGRESS_METHODS.has(frame.method) && notification && typeof notification === "object" && !Array.isArray(notification)) {
        const params = notification as Record<string, unknown>;
        const jobId = params.job_id ?? params.op_id;
        const request = [...this.pending.values()].find(request => request.owner === owner && request.progress?.job_id === jobId);
        if (request?.progress && (params.repo_id === undefined || params.repo_id === request.progress.repo_id)) {
          // The request owns repository identity; native completion/line events
          // omit it. Never let another channel/job update renderer operation state.
          this.emit("progress", { method: frame.method, params: { ...params, repo_id: request.progress.repo_id } });
        }
      }
      this.emit("notification", { profileId: owner.profileId, method: frame.method, params: notification });
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
