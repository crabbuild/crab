/**
 * Response caching for remote SSH operations.
 *
 * Implements TTL-based caching with adaptive TTL based on connection
 * latency, automatic invalidation on write operations, and prefetching
 * for commonly needed data when a remote repo is opened.
 */

import type { RpcRouter } from "./ssh-router";

// ── Types ────────────────────────────────────────────────────────

interface CacheEntry<T = unknown> {
  data: T;
  expiresAt: number;
  fetchedAt: number;
}

interface LatencyBucket {
  statusTtlMs: number;
  refsTtlMs: number;
  summaryTtlMs: number;
}

export interface CacheStats {
  hits: number;
  misses: number;
  evictions: number;
  size: number;
}

// ── Constants ────────────────────────────────────────────────────

/** Default TTLs for different data types (milliseconds). */
const DEFAULT_TTLS = {
  repo_summary: 5_000,
  list_refs: 5_000,
  git_status_porcelain: 3_000,
  hydration_status: 5_000,
} as const;

/** Methods that mutate state and trigger cache invalidation. */
const WRITE_METHODS = new Set([
  "git_commit",
  "git_commit_changes",
  "git_stage_hunks",
  "git_unstage_hunks",
  "git_discard_hunks",
  "git_add_paths",
  "git_reset_paths",
  "git_checkout",
  "git_branch_create",
  "git_branch_delete",
  "git_branch_rename",
  "git_cherry_pick",
  "git_revert",
  "git_merge",
  "git_rebase",
  "git_stash_save",
  "git_stash_apply",
  "git_stash_pop",
  "git_stash_drop",
  "git_fetch_all",
  "git_fetch_branch",
  "git_pull_branch",
  "git_push_branch",
  "op_start",
  "op_continue",
  "op_abort",
  "write_text",
  "delete_file",
  "rename_file",
  "add",
  "hydrate",
  "dehydrate",
  "push",
]);

/**
 * Adaptive TTL multipliers based on connection latency.
 * Higher latency connections get longer cache TTLs to reduce
 * perceived lag from frequent round-trips.
 */
const LATENCY_BUCKETS: Array<{ maxRttMs: number; bucket: LatencyBucket }> = [
  { maxRttMs: 50, bucket: { statusTtlMs: 3_000, refsTtlMs: 5_000, summaryTtlMs: 5_000 } },
  { maxRttMs: 100, bucket: { statusTtlMs: 5_000, refsTtlMs: 10_000, summaryTtlMs: 10_000 } },
  { maxRttMs: 200, bucket: { statusTtlMs: 10_000, refsTtlMs: 15_000, summaryTtlMs: 15_000 } },
  { maxRttMs: Infinity, bucket: { statusTtlMs: 15_000, refsTtlMs: 30_000, summaryTtlMs: 30_000 } },
];

// ── Remote Cache ─────────────────────────────────────────────────

export class RemoteCache {
  private cache = new Map<string, CacheEntry>();
  private latencies = new Map<string, number>();
  private stats: CacheStats = { hits: 0, misses: 0, evictions: 0, size: 0 };

  /**
   * Get a cached value or fetch it using the provided fetcher function.
   * Uses adaptive TTL based on the connection's measured latency.
   */
  async getOrFetch<T>(
    key: string,
    fetcher: () => Promise<T>,
    ttlMs?: number,
  ): Promise<T> {
    const cached = this.cache.get(key);
    if (cached && cached.expiresAt > Date.now()) {
      this.stats.hits++;
      return cached.data as T;
    }

    this.stats.misses++;
    const data = await fetcher();
    const effectiveTtl = ttlMs ?? this.getDefaultTtl(key);

    this.cache.set(key, {
      data,
      expiresAt: Date.now() + effectiveTtl,
      fetchedAt: Date.now(),
    });
    this.stats.size = this.cache.size;

    return data;
  }

  /**
   * Check if a method is a write operation that should trigger
   * cache invalidation.
   */
  isWriteMethod(method: string): boolean {
    return WRITE_METHODS.has(method);
  }

  /**
   * Invalidate all cached entries for a specific repo.
   * Called after any write operation on that repo.
   */
  invalidateRepo(repoId: string): void {
    let evicted = 0;
    for (const key of this.cache.keys()) {
      if (key.startsWith(repoId + ":") || key === repoId) {
        this.cache.delete(key);
        evicted++;
      }
    }
    this.stats.evictions += evicted;
    this.stats.size = this.cache.size;
  }

  /**
   * Invalidate all cached entries for a specific profile.
   * Called on disconnect or when the connection state changes.
   */
  invalidateProfile(profileId: string): void {
    const prefix = `ssh://${profileId}`;
    let evicted = 0;
    for (const key of this.cache.keys()) {
      if (key.startsWith(prefix)) {
        this.cache.delete(key);
        evicted++;
      }
    }
    this.stats.evictions += evicted;
    this.stats.size = this.cache.size;
  }

  /**
   * Force invalidation of all cached data. Triggered by manual
   * refresh (Cmd+R) or explicit user action.
   */
  invalidateAll(): void {
    this.stats.evictions += this.cache.size;
    this.cache.clear();
    this.stats.size = 0;
  }

  /**
   * Update the measured latency for a profile. Used to compute
   * adaptive TTLs.
   */
  updateLatency(profileId: string, latencyMs: number): void {
    this.latencies.set(profileId, latencyMs);
  }

  /**
   * Get the current latency measurement for a profile.
   */
  getLatency(profileId: string): number | undefined {
    return this.latencies.get(profileId);
  }

  /**
   * Get the adaptive TTL for a given method based on the profile's
   * measured latency.
   */
  getAdaptiveTtl(profileId: string, method: string): number {
    const latency = this.latencies.get(profileId) ?? 0;
    const bucket = LATENCY_BUCKETS.find((b) => latency <= b.maxRttMs)?.bucket
      ?? LATENCY_BUCKETS[LATENCY_BUCKETS.length - 1].bucket;

    switch (method) {
      case "git_status_porcelain":
        return bucket.statusTtlMs;
      case "list_refs":
        return bucket.refsTtlMs;
      case "repo_summary":
      case "hydration_status":
        return bucket.summaryTtlMs;
      default:
        return bucket.summaryTtlMs;
    }
  }

  /**
   * Prefetch commonly needed data when a remote repo is opened.
   * Fetches repo_summary, list_refs, git_status_porcelain, and
   * hydration_status in parallel using batch RPC.
   */
  async prefetch(router: RpcRouter, repoId: string): Promise<void> {
    const methods = ["repo_summary", "list_refs", "git_status_porcelain", "hydration_status"];

    const calls = methods.map((method) => ({
      method,
      params: { repo_id: repoId },
    }));

    try {
      const results = await router.batchRpc(repoId, calls);

      for (let i = 0; i < methods.length; i++) {
        const method = methods[i];
        const result = results[i];
        if (result.result !== undefined) {
          const data = result.result;
          // Reconnect fallback is an observation of the old cache, not a fetch.
          // Refreshing its TTL here would present stale repository state as new.
          if (data !== null && typeof data === "object" && "__stale" in data && data.__stale === true) continue;
          const key = `${repoId}:${method}`;
          const ttl = this.getDefaultTtl(key);
          this.cache.set(key, {
            data,
            expiresAt: Date.now() + ttl,
            fetchedAt: Date.now(),
          });
        }
      }
      this.stats.size = this.cache.size;
    } catch {
      // Prefetch failures are non-fatal — the data will be fetched
      // on demand when the UI requests it.
    }
  }

  /**
   * Get cache statistics for diagnostics.
   */
  getStats(): CacheStats {
    return { ...this.stats };
  }

  /**
   * Retrieve a cached value without fetching, even if expired.
   * Returns `{ data, stale }` where `stale` is true if the entry
   * has expired but still has data available. Returns `null` if no
   * cached entry exists for the key.
   *
   * Used during reconnection to serve stale data with a "stale"
   * indicator rather than failing the request entirely.
   */
  getCached<T>(key: string): { data: T; stale: boolean } | null {
    const entry = this.cache.get(key);
    if (!entry) return null;

    const stale = entry.expiresAt <= Date.now();
    if (!stale) this.stats.hits++;
    return { data: entry.data as T, stale };
  }

  /**
   * Remove expired entries. Called periodically or on demand.
   */
  evictExpired(): void {
    const now = Date.now();
    let evicted = 0;
    for (const [key, entry] of this.cache) {
      if (entry.expiresAt <= now) {
        this.cache.delete(key);
        evicted++;
      }
    }
    this.stats.evictions += evicted;
    this.stats.size = this.cache.size;
  }

  // ── Private ────────────────────────────────────────────────────

  private getDefaultTtl(key: string): number {
    // Extract method from cache key format "repoId:method"
    const colonIdx = key.lastIndexOf(":");
    const method = colonIdx >= 0 ? key.slice(colonIdx + 1) : key;

    if (method in DEFAULT_TTLS) {
      return DEFAULT_TTLS[method as keyof typeof DEFAULT_TTLS];
    }
    return 5_000; // Default 5s TTL
  }
}
