/**
 * Tests for prefetching on repo open.
 *
 * Verifies that when a remote repo is opened via `openAsRepo`, the
 * cache is warmed with `repo_summary`, `list_refs`, `git_status_porcelain`,
 * and `hydration_status` via a single batch RPC call.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { RemoteFileBrowser } from "../electron/ssh-file-browser";
import { RemoteCache } from "../electron/ssh-cache";

// ── Mocks ────────────────────────────────────────────────────────

function createMockSessionManager(status = "connected") {
  return {
    getStatus: vi.fn().mockReturnValue(status),
    getConnection: vi.fn(),
  };
}

function createMockRouter() {
  return {
    sendToRemoteAgent: vi.fn().mockResolvedValue({
      repo_id: "ssh://prof1/home/user/ml-repo",
      is_remote: true,
      path: "/home/user/ml-repo",
    }),
    batchRpc: vi.fn().mockResolvedValue([
      { result: { name: "ml-repo", branch: "main" } },
      { result: { branches: ["main", "dev", "feature/train"] } },
      { result: { modified: 2, staged: 1 } },
      { result: { hydrated: 5, dehydrated: 3 } },
    ]),
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("Prefetch on repo open", () => {
  let router: ReturnType<typeof createMockRouter>;
  let sessionManager: ReturnType<typeof createMockSessionManager>;
  let cache: RemoteCache;

  beforeEach(() => {
    router = createMockRouter();
    sessionManager = createMockSessionManager();
    cache = new RemoteCache();
  });

  it("triggers batch prefetch after opening a remote repo", async () => {
    const browser = new RemoteFileBrowser(router as any, sessionManager as any, cache);

    await browser.openAsRepo("prof1", "/home/user/ml-repo");

    // Wait for the background prefetch to complete
    await vi.waitFor(() => {
      expect(router.batchRpc).toHaveBeenCalledOnce();
    });

    // Verify the batch call was made with the correct repo ID and methods
    expect(router.batchRpc).toHaveBeenCalledWith(
      "ssh://prof1/home/user/ml-repo",
      expect.arrayContaining([
        expect.objectContaining({ method: "repo_summary" }),
        expect.objectContaining({ method: "list_refs" }),
        expect.objectContaining({ method: "git_status_porcelain" }),
        expect.objectContaining({ method: "hydration_status" }),
      ]),
    );
  });

  it("populates cache so subsequent reads are cache hits", async () => {
    const browser = new RemoteFileBrowser(router as any, sessionManager as any, cache);

    await browser.openAsRepo("prof1", "/home/user/ml-repo");

    // Wait for the background prefetch to fully complete (batchRpc resolves
    // and cache entries are written). Since prefetch is fire-and-forget,
    // we need to wait for the async chain to settle.
    await vi.waitFor(async () => {
      const repoId = "ssh://prof1/home/user/ml-repo";
      const cached = cache.getCached(`${repoId}:repo_summary`);
      expect(cached).not.toBeNull();
    });

    // Verify cache was populated — fetcher should NOT be called
    const fetcher = vi.fn().mockResolvedValue("should-not-be-called");
    const repoId = "ssh://prof1/home/user/ml-repo";

    const summary = await cache.getOrFetch(`${repoId}:repo_summary`, fetcher);
    const refs = await cache.getOrFetch(`${repoId}:list_refs`, fetcher);
    const status = await cache.getOrFetch(`${repoId}:git_status_porcelain`, fetcher);
    const hydration = await cache.getOrFetch(`${repoId}:hydration_status`, fetcher);

    expect(summary).toEqual({ name: "ml-repo", branch: "main" });
    expect(refs).toEqual({ branches: ["main", "dev", "feature/train"] });
    expect(status).toEqual({ modified: 2, staged: 1 });
    expect(hydration).toEqual({ hydrated: 5, dehydrated: 3 });
    expect(fetcher).not.toHaveBeenCalled();
  });

  it("returns the repo handle immediately without waiting for prefetch", async () => {
    // Make prefetch slow to verify it doesn't block the return
    router.batchRpc.mockImplementation(
      () => new Promise((resolve) => setTimeout(() => resolve([
        { result: {} },
        { result: {} },
        { result: {} },
        { result: {} },
      ]), 500)),
    );

    const browser = new RemoteFileBrowser(router as any, sessionManager as any, cache);
    const start = Date.now();

    const result = await browser.openAsRepo("prof1", "/home/user/ml-repo");

    // The open_local call should return immediately
    const elapsed = Date.now() - start;
    expect(elapsed).toBeLessThan(100);
    expect(result).toEqual({
      repo_id: "ssh://prof1/home/user/ml-repo",
      is_remote: true,
      path: "/home/user/ml-repo",
    });
  });

  it("does not throw when prefetch fails", async () => {
    router.batchRpc.mockRejectedValue(new Error("Network timeout"));

    const browser = new RemoteFileBrowser(router as any, sessionManager as any, cache);

    // Should not throw despite prefetch failure
    const result = await browser.openAsRepo("prof1", "/home/user/ml-repo");
    expect(result).toBeDefined();

    // Wait a tick to ensure the rejected promise is handled
    await new Promise((r) => setTimeout(r, 10));
  });

  it("opens the canonical remote repository identity without a cache", async () => {
    const browser = new RemoteFileBrowser(router as any, sessionManager as any);

    const result = await browser.openAsRepo("prof1", "/home/user/ml-repo");

    expect(result).toBeDefined();
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("prof1", "open_local", {
      repo_id: "ssh://prof1/home/user/ml-repo", path: "/home/user/ml-repo",
    });
    // batchRpc should NOT be called when no cache is provided
    expect(router.batchRpc).not.toHaveBeenCalled();
  });

  it("projects a server-local workspace as remote for renderer worktree routing", async () => {
    const repoId = "ssh://prof1/home/user/ml-repo";
    router.sendToRemoteAgent.mockResolvedValueOnce({ repo_id: repoId, is_remote: false, path: "/home/user/ml-repo" });
    const browser = new RemoteFileBrowser(
      router as unknown as ConstructorParameters<typeof RemoteFileBrowser>[0],
      sessionManager as unknown as ConstructorParameters<typeof RemoteFileBrowser>[1],
    );
    const opened = await browser.openAsRepo("prof1", "/home/user/ml-repo");
    expect(opened).toEqual({ repo_id: repoId, is_remote: true, path: "/home/user/ml-repo" });
  });

  it("constructs the correct repo ID from profileId and path", async () => {
    const browser = new RemoteFileBrowser(router as any, sessionManager as any, cache);

    await browser.openAsRepo("gpu-server-42", "/data/models/llama");

    await vi.waitFor(() => {
      expect(router.batchRpc).toHaveBeenCalled();
    });

    expect(router.batchRpc).toHaveBeenCalledWith(
      "ssh://gpu-server-42/data/models/llama",
      expect.any(Array),
    );
  });
});
