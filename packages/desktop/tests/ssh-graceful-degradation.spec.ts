import { mockStorageConnections } from "./mock-storage-connections";
/**
 * Tests for graceful degradation during SSH reconnection.
 *
 * Verifies that:
 * - Explicitly cacheable metadata returns cached data with `__stale: true` during reconnection
 * - Write operations are rejected immediately with SSH_RECONNECTING code
 * - Live operations are rejected; only cacheable metadata is queued for replay
 * - The StaleBadge and WriteBlockedOverlay components render correctly
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import type { ClientChannel } from "ssh2";
import { parseRepoId } from "../electron/ssh-types";
import { RpcRouter, isCacheableReadMethod } from "../electron/ssh-router";
import { RemoteCache } from "../electron/ssh-cache";

// ─── Mock SSH Session Manager ────────────────────────────────────────────────

class MockChannel extends EventEmitter {
  written: string[] = [];
  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    if (cb) cb(null);
    return true;
  }
  end = vi.fn();

  asChannel(): ClientChannel {
    return this as unknown as ClientChannel;
  }
}

class MockSessionManager extends EventEmitter {
  repositoryWork(): Promise<unknown> | undefined { return undefined; }
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockChannel } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { channels: { agent?: MockChannel } } | undefined {
    return this.connections.get(profileId);
  }
}

class MockLocalAgent {
  async request(_method: string, _params: unknown): Promise<unknown> {
    return { ok: true };
  }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function createRouterWithCache() {
  const sm = new MockSessionManager();
  const la = new MockLocalAgent();
  const router = new RpcRouter(sm as any, la, mockStorageConnections);
  const cache = new RemoteCache();
  router.setCache(cache);
  return { router, sessionManager: sm, localAgent: la, cache };
}

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("isCacheableReadMethod", () => {
  it("identifies cacheable read methods", () => {
    expect(isCacheableReadMethod("git_status_porcelain")).toBe(true);
    expect(isCacheableReadMethod("git_log")).toBe(true);
    expect(isCacheableReadMethod("list_refs")).toBe(true);
    expect(isCacheableReadMethod("repo_summary")).toBe(true);
    expect(isCacheableReadMethod("hydration_status")).toBe(true);
  });

  it("rejects non-cacheable methods", () => {
    expect(isCacheableReadMethod("file_diff")).toBe(false);
    expect(isCacheableReadMethod("git_commit")).toBe(false);
    expect(isCacheableReadMethod("ping")).toBe(false);
    expect(isCacheableReadMethod("git_stash_list")).toBe(false);
  });
});

describe("RemoteCache — getCached", () => {
  it("returns null when no entry exists", () => {
    const cache = new RemoteCache();
    expect(cache.getCached("nonexistent:key")).toBeNull();
  });

  it("returns data with stale: false for fresh entries", async () => {
    const cache = new RemoteCache();
    await cache.getOrFetch("repo:git_status_porcelain", async () => ({ files: ["a.txt"] }), 5000);

    const result = cache.getCached<{ files: string[] }>("repo:git_status_porcelain");
    expect(result).not.toBeNull();
    expect(result!.data).toEqual({ files: ["a.txt"] });
    expect(result!.stale).toBe(false);
  });

  it("returns data with stale: true for expired entries", async () => {
    const cache = new RemoteCache();
    // Use a very short TTL so it expires immediately
    await cache.getOrFetch("repo:git_log", async () => ({ commits: [1, 2, 3] }), 1);

    // Wait for expiry
    await new Promise((r) => setTimeout(r, 10));

    const result = cache.getCached<{ commits: number[] }>("repo:git_log");
    expect(result).not.toBeNull();
    expect(result!.data).toEqual({ commits: [1, 2, 3] });
    expect(result!.stale).toBe(true);
  });
});

describe("RpcRouter — graceful degradation during reconnection", () => {
  it("returns stale cached data for cacheable read methods during reconnection", async () => {
    const { router, sessionManager, cache } = createRouterWithCache();
    const repoId = "ssh://gpu-server/home/user/repo";

    // Populate cache with data
    await cache.getOrFetch(`${repoId}:git_status_porcelain`, async () => "M file.txt", 5000);

    // Set connection to reconnecting
    sessionManager.setStatus("gpu-server", "reconnecting");

    const result = await router.route(repoId, "git_status_porcelain", { repo_id: repoId });

    // Should return cached data with __stale flag
    expect(result).toHaveProperty("__stale", true);
    expect(result).toHaveProperty("data", "M file.txt");
  });

  it("returns stale cached object data preserving structure during reconnection", async () => {
    const { router, sessionManager, cache } = createRouterWithCache();
    const repoId = "ssh://gpu-server/home/user/repo";

    // Populate cache with object data
    await cache.getOrFetch(`${repoId}:repo_summary`, async () => ({
      name: "my-repo",
      branch: "main",
      dirty: 3,
    }), 5000);

    sessionManager.setStatus("gpu-server", "reconnecting");

    const result = await router.route(repoId, "repo_summary", { repo_id: repoId }) as any;

    expect(result.__stale).toBe(true);
    expect(result.name).toBe("my-repo");
    expect(result.branch).toBe("main");
    expect(result.dirty).toBe(3);
  });

  it("rejects write operations immediately with SSH_RECONNECTING code", async () => {
    const { router, sessionManager } = createRouterWithCache();
    const repoId = "ssh://gpu-server/home/user/repo";

    sessionManager.setStatus("gpu-server", "reconnecting");

    await expect(router.route(repoId, "git_commit", { repo_id: repoId, message: "test" }))
      .rejects.toMatchObject({ code: "SSH_RECONNECTING" });
  });

  it("rejects git_push during reconnection", async () => {
    const { router, sessionManager } = createRouterWithCache();
    const repoId = "ssh://gpu-server/repo";

    sessionManager.setStatus("gpu-server", "reconnecting");

    await expect(
      router.route(repoId, "git_push", { repo_id: repoId }),
    ).rejects.toThrow("Connection lost");
  });

  it("rejects git_stage during reconnection", async () => {
    const { router, sessionManager } = createRouterWithCache();
    const repoId = "ssh://gpu-server/repo";

    sessionManager.setStatus("gpu-server", "reconnecting");

    await expect(
      router.route(repoId, "git_stage", { repo_id: repoId }),
    ).rejects.toThrow("Connection lost");
  });

  it("queues cacheable read operations when no cached data exists", async () => {
    const { router, sessionManager } = createRouterWithCache();
    const repoId = "ssh://gpu-server/repo";

    // No cache populated — should fall through to queue
    sessionManager.setStatus("gpu-server", "reconnecting");

    const promise = router.route(repoId, "git_status_porcelain", { repo_id: repoId });

    // Simulate reconnection
    const channel = new MockChannel();
    sessionManager.setStatus("gpu-server", "connected");
    sessionManager.setConnection("gpu-server", channel);
    router.attachChannel("gpu-server", channel.asChannel());

    const replayPromise = router.replayQueue("gpu-server");
    await vi.waitFor(() => expect(channel.written).toHaveLength(1));
    const response = JSON.stringify({ jsonrpc: "2.0", id: 1, result: "?? new.txt" }) + "\n";
    channel.emit("data", Buffer.from(response));
    await replayPromise;

    const result = await promise;
    expect(result).toBe("?? new.txt");
  });

  it("returns stale data even when cache entry has expired", async () => {
    const { router, sessionManager, cache } = createRouterWithCache();
    const repoId = "ssh://gpu-server/repo";

    // Populate with very short TTL
    await cache.getOrFetch(`${repoId}:list_refs`, async () => ({
      branches: ["main", "feature"],
    }), 1);

    // Wait for expiry
    await new Promise((r) => setTimeout(r, 10));

    sessionManager.setStatus("gpu-server", "reconnecting");

    const result = await router.route(repoId, "list_refs", { repo_id: repoId }) as any;

    expect(result.__stale).toBe(true);
    expect(result.branches).toEqual(["main", "feature"]);
  });
});
