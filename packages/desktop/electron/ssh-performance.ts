/**
 * Performance Optimizations for SSH remote connections.
 *
 * Implements:
 * - Prefetching on repo open (batch RPC)
 * - Delta updates for git status (only send changes)
 * - Connection pooling (reuse SSH connection for multiple repos)
 * - Lazy notification subscription (only forward for visible repos)
 * - Response streaming for large payloads
 * - Compression management
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { RemoteCache } from "./ssh-cache";
import { StatusDeltaTracker, requestWithDelta, type StatusEntry } from "./ssh-status-delta";

// ── Types ────────────────────────────────────────────────────────

export interface PrefetchResult {
  repoId: string;
  methods: string[];
  durationMs: number;
  cached: number;
}

/** @deprecated Use StatusDeltaTracker from ssh-status-delta.ts instead. */
export interface DeltaState {
  lastHash: string;
  lastEntries: unknown[];
  lastFetchedAt: number;
}

export interface StreamChunk {
  index: number;
  total?: number;
  data: unknown;
  done: boolean;
}

export interface PerformanceStats {
  totalRpcCalls: number;
  cacheHits: number;
  cacheMisses: number;
  avgLatencyMs: number;
  prefetchesDone: number;
  deltaUpdates: number;
  bytesTransferred: number;
  compressionRatio?: number;
}

export interface ConnectionPoolEntry {
  profileId: string;
  repoIds: Set<string>;
  lastUsedAt: number;
}

// ── Performance Manager ──────────────────────────────────────────

export class PerformanceManager extends EventEmitter {
  private router: RpcRouter;
  private cache: RemoteCache;
  private deltaTracker: StatusDeltaTracker;
  private subscriptions = new Map<string, Set<string>>();
  private connectionPool = new Map<string, ConnectionPoolEntry>();
  private stats: PerformanceStats = {
    totalRpcCalls: 0,
    cacheHits: 0,
    cacheMisses: 0,
    avgLatencyMs: 0,
    prefetchesDone: 0,
    deltaUpdates: 0,
    bytesTransferred: 0,
  };

  constructor(router: RpcRouter, cache: RemoteCache) {
    super();
    this.router = router;
    this.cache = cache;
    this.deltaTracker = new StatusDeltaTracker();
  }

  // ── Prefetching ──────────────────────────────────────────────

  /**
   * Prefetch commonly needed data when a remote repo is opened.
   * Uses batch RPC to fetch repo_summary, list_refs, git_status_porcelain,
   * and hydration_status in a single round-trip.
   */
  async prefetchOnOpen(repoId: string): Promise<PrefetchResult> {
    const methods = [
      "repo_summary",
      "list_refs",
      "git_status_porcelain",
      "hydration_status",
    ];

    const start = Date.now();

    try {
      await this.cache.prefetch(this.router, repoId);
      this.stats.prefetchesDone++;

      return {
        repoId,
        methods,
        durationMs: Date.now() - start,
        cached: methods.length,
      };
    } catch {
      return {
        repoId,
        methods,
        durationMs: Date.now() - start,
        cached: 0,
      };
    }
  }

  // ── Delta Updates ────────────────────────────────────────────

  /**
   * Request a delta update for git_status_porcelain. Uses the
   * StatusDeltaTracker to include last_hash in the request and
   * process delta responses from the remote agent.
   * Falls back to full response if delta is not available.
   */
  async getDeltaStatus(repoId: string): Promise<{
    entries: unknown[];
    isDelta: boolean;
  }> {
    const lastHash = this.deltaTracker.getLastHash(repoId);

    try {
      const entries = await requestWithDelta(
        this.router,
        this.deltaTracker,
        repoId,
        { repo_id: repoId },
      );

      const newHash = this.deltaTracker.getLastHash(repoId);
      const isDelta = lastHash !== null && newHash !== lastHash;
      if (isDelta) {
        this.stats.deltaUpdates++;
      }

      return { entries, isDelta };
    } catch {
      // Fall back to regular status
      const result = await this.router.route(repoId, "git_status_porcelain", {
        repo_id: repoId,
      }) as { entries: StatusEntry[] };

      return { entries: result.entries, isDelta: false };
    }
  }

  /**
   * Get the underlying StatusDeltaTracker instance for direct access.
   */
  getDeltaTracker(): StatusDeltaTracker {
    return this.deltaTracker;
  }

  // ── Connection Pooling ───────────────────────────────────────

  /**
   * Register a repo as using a specific profile's connection.
   * Multiple repos can share the same SSH connection and agent instance.
   */
  registerRepoConnection(profileId: string, repoId: string): void {
    let entry = this.connectionPool.get(profileId);
    if (!entry) {
      entry = { profileId, repoIds: new Set(), lastUsedAt: Date.now() };
      this.connectionPool.set(profileId, entry);
    }
    entry.repoIds.add(repoId);
    entry.lastUsedAt = Date.now();
  }

  /**
   * Unregister a repo from a connection pool entry.
   */
  unregisterRepoConnection(profileId: string, repoId: string): void {
    const entry = this.connectionPool.get(profileId);
    if (entry) {
      entry.repoIds.delete(repoId);
    }
  }

  /**
   * Get all repos sharing a connection.
   */
  getPooledRepos(profileId: string): string[] {
    const entry = this.connectionPool.get(profileId);
    return entry ? [...entry.repoIds] : [];
  }

  // ── Lazy Notification Subscription ───────────────────────────

  /**
   * Subscribe to notifications for a repo (when it becomes visible
   * in the renderer). Only subscribed repos receive forwarded
   * notifications, reducing SSH channel traffic.
   */
  subscribe(profileId: string, repoId: string): void {
    let subs = this.subscriptions.get(profileId);
    if (!subs) {
      subs = new Set();
      this.subscriptions.set(profileId, subs);
    }
    subs.add(repoId);
  }

  /**
   * Unsubscribe from notifications for a repo (when it's hidden
   * or closed in the renderer).
   */
  unsubscribe(profileId: string, repoId: string): void {
    const subs = this.subscriptions.get(profileId);
    if (subs) {
      subs.delete(repoId);
    }
  }

  /**
   * Check if a notification should be forwarded to the renderer.
   */
  shouldForwardNotification(profileId: string, repoId: string): boolean {
    const subs = this.subscriptions.get(profileId);
    return subs ? subs.has(repoId) : false;
  }

  // ── Response Streaming ───────────────────────────────────────

  /**
   * Stream a large response (e.g., git_log with many commits) by
   * requesting chunks incrementally. The UI can render progressively
   * as chunks arrive.
   */
  async *streamResponse(
    repoId: string,
    method: string,
    params: unknown,
    chunkSize = 50,
  ): AsyncGenerator<StreamChunk> {
    let offset = 0;
    let done = false;

    while (!done) {
      const result = await this.router.route(repoId, method, {
        ...params as object,
        _offset: offset,
        _limit: chunkSize,
      }) as { data: unknown[]; total: number; hasMore: boolean };

      yield {
        index: offset,
        total: result.total,
        data: result.data,
        done: !result.hasMore,
      };

      offset += result.data.length;
      done = !result.hasMore;
      this.stats.bytesTransferred += JSON.stringify(result.data).length;
    }
  }

  // ── Compression ──────────────────────────────────────────────

  /**
   * Get compression statistics for a profile's SSH channel.
   * Only meaningful when compression is enabled on the profile.
   */
  getCompressionStats(_profileId: string): { enabled: boolean; ratio?: number } {
    // Compression is handled at the SSH transport level by ssh2.
    // We track the ratio based on observed payload sizes.
    const ratio = this.stats.compressionRatio;
    return {
      enabled: ratio !== undefined,
      ratio,
    };
  }

  // ── Statistics ───────────────────────────────────────────────

  /**
   * Record an RPC call for statistics tracking.
   */
  recordCall(latencyMs: number, bytesReceived: number, cacheHit: boolean): void {
    this.stats.totalRpcCalls++;
    this.stats.bytesTransferred += bytesReceived;

    if (cacheHit) {
      this.stats.cacheHits++;
    } else {
      this.stats.cacheMisses++;
    }

    // Rolling average latency
    const n = this.stats.totalRpcCalls;
    this.stats.avgLatencyMs = this.stats.avgLatencyMs * ((n - 1) / n) + latencyMs / n;
  }

  /**
   * Get current performance statistics.
   */
  getStats(): PerformanceStats {
    return { ...this.stats };
  }

  /**
   * Reset statistics (e.g., for a new measurement window).
   */
  resetStats(): void {
    this.stats = {
      totalRpcCalls: 0,
      cacheHits: 0,
      cacheMisses: 0,
      avgLatencyMs: 0,
      prefetchesDone: 0,
      deltaUpdates: 0,
      bytesTransferred: 0,
    };
  }
}
