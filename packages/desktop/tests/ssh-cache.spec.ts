/**
 * Unit tests for the RemoteCache module.
 *
 * Tests cache hit/miss, TTL expiry, adaptive TTL adjustment,
 * invalidation on write, and prefetch.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { RemoteCache } from "../electron/ssh-cache";

describe("RemoteCache", () => {
  let cache: RemoteCache;

  beforeEach(() => {
    cache = new RemoteCache();
  });

  describe("getOrFetch", () => {
    it("returns fetched data on cache miss", async () => {
      const fetcher = vi.fn().mockResolvedValue({ branches: ["main"] });

      const result = await cache.getOrFetch("repo:list_refs", fetcher, 5000);

      expect(result).toEqual({ branches: ["main"] });
      expect(fetcher).toHaveBeenCalledOnce();
    });

    it("returns cached data on cache hit without calling fetcher", async () => {
      const fetcher = vi.fn().mockResolvedValue({ branches: ["main"] });

      await cache.getOrFetch("repo:list_refs", fetcher, 5000);
      const result = await cache.getOrFetch("repo:list_refs", fetcher, 5000);

      expect(result).toEqual({ branches: ["main"] });
      expect(fetcher).toHaveBeenCalledOnce();
    });

    it("re-fetches after TTL expires", async () => {
      vi.useFakeTimers();
      const fetcher = vi.fn()
        .mockResolvedValueOnce({ count: 1 })
        .mockResolvedValueOnce({ count: 2 });

      await cache.getOrFetch("repo:git_status_porcelain", fetcher, 3000);

      // Advance past TTL
      vi.advanceTimersByTime(3001);

      const result = await cache.getOrFetch("repo:git_status_porcelain", fetcher, 3000);

      expect(result).toEqual({ count: 2 });
      expect(fetcher).toHaveBeenCalledTimes(2);

      vi.useRealTimers();
    });

    it("does not re-fetch before TTL expires", async () => {
      vi.useFakeTimers();
      const fetcher = vi.fn().mockResolvedValue({ data: "cached" });

      await cache.getOrFetch("repo:repo_summary", fetcher, 5000);

      // Advance but not past TTL
      vi.advanceTimersByTime(4999);

      const result = await cache.getOrFetch("repo:repo_summary", fetcher, 5000);

      expect(result).toEqual({ data: "cached" });
      expect(fetcher).toHaveBeenCalledOnce();

      vi.useRealTimers();
    });
  });

  describe("invalidateRepo", () => {
    it("removes all entries for a specific repo", async () => {
      const fetcher1 = vi.fn().mockResolvedValue("status");
      const fetcher2 = vi.fn().mockResolvedValue("refs");
      const fetcher3 = vi.fn().mockResolvedValue("other-repo");

      await cache.getOrFetch("ssh://prof1/repo:git_status_porcelain", fetcher1, 5000);
      await cache.getOrFetch("ssh://prof1/repo:list_refs", fetcher2, 5000);
      await cache.getOrFetch("ssh://prof2/other:list_refs", fetcher3, 5000);

      cache.invalidateRepo("ssh://prof1/repo");

      // These should re-fetch
      const newFetcher1 = vi.fn().mockResolvedValue("new-status");
      const newFetcher2 = vi.fn().mockResolvedValue("new-refs");
      const newFetcher3 = vi.fn().mockResolvedValue("still-cached");

      await cache.getOrFetch("ssh://prof1/repo:git_status_porcelain", newFetcher1, 5000);
      await cache.getOrFetch("ssh://prof1/repo:list_refs", newFetcher2, 5000);
      await cache.getOrFetch("ssh://prof2/other:list_refs", newFetcher3, 5000);

      expect(newFetcher1).toHaveBeenCalledOnce();
      expect(newFetcher2).toHaveBeenCalledOnce();
      // Other repo should still be cached
      expect(newFetcher3).not.toHaveBeenCalled();
    });
  });

  describe("invalidateProfile", () => {
    it("removes all entries for a specific profile", async () => {
      const fetcher1 = vi.fn().mockResolvedValue("data1");
      const fetcher2 = vi.fn().mockResolvedValue("data2");

      await cache.getOrFetch("ssh://profile-a/repo1:status", fetcher1, 5000);
      await cache.getOrFetch("ssh://profile-a/repo2:status", fetcher2, 5000);

      cache.invalidateProfile("profile-a");

      const newFetcher1 = vi.fn().mockResolvedValue("new1");
      const newFetcher2 = vi.fn().mockResolvedValue("new2");

      await cache.getOrFetch("ssh://profile-a/repo1:status", newFetcher1, 5000);
      await cache.getOrFetch("ssh://profile-a/repo2:status", newFetcher2, 5000);

      expect(newFetcher1).toHaveBeenCalledOnce();
      expect(newFetcher2).toHaveBeenCalledOnce();
    });
  });

  describe("invalidateAll", () => {
    it("clears all cached entries", async () => {
      const fetcher = vi.fn().mockResolvedValue("data");

      await cache.getOrFetch("key1", fetcher, 5000);
      await cache.getOrFetch("key2", fetcher, 5000);

      cache.invalidateAll();

      const stats = cache.getStats();
      expect(stats.size).toBe(0);
    });
  });

  describe("isWriteMethod", () => {
    it("identifies write methods correctly", () => {
      expect(cache.isWriteMethod("git_commit")).toBe(true);
      expect(cache.isWriteMethod("git_stage_hunks")).toBe(true);
      expect(cache.isWriteMethod("git_checkout")).toBe(true);
      expect(cache.isWriteMethod("write_text")).toBe(true);
      expect(cache.isWriteMethod("delete_file")).toBe(true);
      expect(cache.isWriteMethod("op_start")).toBe(true);
    });

    it("identifies read methods correctly", () => {
      expect(cache.isWriteMethod("git_status_porcelain")).toBe(false);
      expect(cache.isWriteMethod("list_refs")).toBe(false);
      expect(cache.isWriteMethod("repo_summary")).toBe(false);
      expect(cache.isWriteMethod("read_text")).toBe(false);
      expect(cache.isWriteMethod("git_log")).toBe(false);
    });
  });

  describe("adaptive TTL", () => {
    it("returns short TTLs for low-latency connections", () => {
      cache.updateLatency("fast-server", 30);

      expect(cache.getAdaptiveTtl("fast-server", "git_status_porcelain")).toBe(3000);
      expect(cache.getAdaptiveTtl("fast-server", "list_refs")).toBe(5000);
      expect(cache.getAdaptiveTtl("fast-server", "repo_summary")).toBe(5000);
    });

    it("returns medium TTLs for moderate-latency connections", () => {
      cache.updateLatency("medium-server", 75);

      expect(cache.getAdaptiveTtl("medium-server", "git_status_porcelain")).toBe(5000);
      expect(cache.getAdaptiveTtl("medium-server", "list_refs")).toBe(10000);
      expect(cache.getAdaptiveTtl("medium-server", "repo_summary")).toBe(10000);
    });

    it("returns long TTLs for high-latency connections", () => {
      cache.updateLatency("slow-server", 150);

      expect(cache.getAdaptiveTtl("slow-server", "git_status_porcelain")).toBe(10000);
      expect(cache.getAdaptiveTtl("slow-server", "list_refs")).toBe(15000);
      expect(cache.getAdaptiveTtl("slow-server", "repo_summary")).toBe(15000);
    });

    it("returns maximum TTLs for very high-latency connections", () => {
      cache.updateLatency("very-slow", 300);

      expect(cache.getAdaptiveTtl("very-slow", "git_status_porcelain")).toBe(15000);
      expect(cache.getAdaptiveTtl("very-slow", "list_refs")).toBe(30000);
      expect(cache.getAdaptiveTtl("very-slow", "repo_summary")).toBe(30000);
    });

    it("defaults to lowest bucket when no latency is recorded", () => {
      expect(cache.getAdaptiveTtl("unknown-server", "git_status_porcelain")).toBe(3000);
    });
  });

  describe("evictExpired", () => {
    it("removes expired entries", async () => {
      vi.useFakeTimers();
      const fetcher = vi.fn().mockResolvedValue("data");

      await cache.getOrFetch("short-lived", fetcher, 1000);
      await cache.getOrFetch("long-lived", fetcher, 10000);

      vi.advanceTimersByTime(2000);
      cache.evictExpired();

      const stats = cache.getStats();
      expect(stats.size).toBe(1); // Only long-lived remains

      vi.useRealTimers();
    });
  });

  describe("getStats", () => {
    it("tracks hits and misses", async () => {
      const fetcher = vi.fn().mockResolvedValue("data");

      await cache.getOrFetch("key", fetcher, 5000);
      await cache.getOrFetch("key", fetcher, 5000);
      await cache.getOrFetch("key", fetcher, 5000);

      const stats = cache.getStats();
      expect(stats.misses).toBe(1);
      expect(stats.hits).toBe(2);
      expect(stats.size).toBe(1);
    });

    it("tracks evictions", async () => {
      const fetcher = vi.fn().mockResolvedValue("data");

      await cache.getOrFetch("key1", fetcher, 5000);
      await cache.getOrFetch("key2", fetcher, 5000);

      cache.invalidateAll();

      const stats = cache.getStats();
      expect(stats.evictions).toBe(2);
      expect(stats.size).toBe(0);
    });
  });

  describe("prefetch", () => {
    it("populates cache with batch RPC results", async () => {
      const mockRouter = {
        batchRpc: vi.fn().mockResolvedValue([
          { result: { name: "my-repo" } },
          { result: { branches: ["main", "dev"] } },
          { result: { modified: 3 } },
          { result: { hydrated: true } },
        ]),
      };

      const repoId = "ssh://prof1/home/user/repo";
      await cache.prefetch(mockRouter as any, repoId);

      // Verify cache was populated — fetcher should not be called
      const fetcher = vi.fn().mockResolvedValue("should-not-be-called");

      const summary = await cache.getOrFetch(`${repoId}:repo_summary`, fetcher, 5000);
      const refs = await cache.getOrFetch(`${repoId}:list_refs`, fetcher, 5000);
      const status = await cache.getOrFetch(`${repoId}:git_status_porcelain`, fetcher, 5000);
      const hydration = await cache.getOrFetch(`${repoId}:hydration_status`, fetcher, 5000);

      expect(summary).toEqual({ name: "my-repo" });
      expect(refs).toEqual({ branches: ["main", "dev"] });
      expect(status).toEqual({ modified: 3 });
      expect(hydration).toEqual({ hydrated: true });
      expect(fetcher).not.toHaveBeenCalled();
    });

    it("handles prefetch failures gracefully", async () => {
      const mockRouter = {
        batchRpc: vi.fn().mockRejectedValue(new Error("Connection lost")),
      };

      // Should not throw
      await expect(
        cache.prefetch(mockRouter as any, "ssh://prof1/repo"),
      ).resolves.toBeUndefined();
    });
  });
});
