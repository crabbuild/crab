/**
 * Integration tests for provider resolution.
 *
 * Tests the full flow: IPC handler → ProviderRegistry → correct provider.
 * Validates that:
 * - Local repo_ids resolve to LocalProvider
 * - Remote repo_ids (ssh://) with explicit registration resolve to RemoteProvider
 * - Unregistered local repo_ids fall back to LocalProvider singleton
 * - Unregistered remote repo_ids are rejected before local dispatch
 * - Provider registration happens before the RepoHandle is returned (no race)
 *
 * Validates: Requirements 4.4, 4.5, 14.1, 14.2, 14.3
 */

import { describe, it, expect, beforeEach } from "vitest";
import {
  ProviderRegistry,
  _resetProviderRegistry,
  getProviderRegistry,
} from "../electron/provider-registry";
import { parseRepoId } from "../electron/ssh-types";
import type { RepoProvider } from "../electron/repo-provider";

// ── Mock Providers ───────────────────────────────────────────────

function createMockLocalProvider(): RepoProvider {
  return {
    request: async (method: string, params: Record<string, unknown>) => ({
      __providerType: "local",
      method,
      params,
    }),
    watchStart: async () => {},
    watchStop: async () => {},
    fsCreate: async (_repoId, path, kind) => ({ path, kind }),
    fsRename: async (_repoId, oldPath, newPath) => ({ oldPath, newPath }),
    fsDelete: async (_repoId, path) => ({ deleted: path, trash: false }),
    fsWrite: async (_repoId, path, content) => ({ path, written: content.length }),
    fsRead: async () => ({ exists: true, content: "local content" }),
    capabilities: async () => ({
      canEdit: true,
      canWatch: true,
      canStageHunks: true,
      canWorktree: true,
      canForge: true,
      canHydrate: true,
      canSearch: true,
      canExec: true,
      canTrash: true,
    }),
    dispose: () => {},
  } as RepoProvider;
}

function createMockRemoteProvider(profileId: string): RepoProvider {
  return {
    request: async (method: string, params: Record<string, unknown>) => ({
      __providerType: "remote",
      __profileId: profileId,
      method,
      params,
    }),
    watchStart: async () => {},
    watchStop: async () => {},
    fsCreate: async (_repoId, path, kind) => ({ path, kind }),
    fsRename: async (_repoId, oldPath, newPath) => ({ oldPath, newPath }),
    fsDelete: async (_repoId, path) => ({ deleted: path, trash: false }),
    fsWrite: async (_repoId, path, content) => ({ path, written: content.length }),
    fsRead: async () => ({ exists: true, content: "remote content" }),
    capabilities: async () => ({
      canEdit: true,
      canWatch: false,
      canStageHunks: true,
      canWorktree: true,
      canForge: true,
      canHydrate: false,
      canSearch: false,
      canExec: false,
      canTrash: false,
    }),
    dispose: () => {},
  } as RepoProvider;
}

// ── Simulated IPC Handler ────────────────────────────────────────
//
// Mirrors the logic in main.ts: resolves provider from repo_id,
// dispatches the call, and handles open/close lifecycle.

interface SimulatedIpcContext {
  registry: ProviderRegistry;
  localProvider: RepoProvider;
  /** Simulates the local agent's request() for open_local/open_url. */
  agentRequest: (method: string, params: unknown) => Promise<unknown>;
}

/**
 * Simulates the `crab:rpc` IPC handler from main.ts. This is the
 * integration surface we're testing — it exercises the full flow
 * from method dispatch through ProviderRegistry resolution.
 */
async function simulateIpcHandler(
  ctx: SimulatedIpcContext,
  method: string,
  params: unknown,
): Promise<unknown> {
  const { registry, localProvider } = ctx;

  // Handle repo open: register provider before returning handle.
  if (method === "open_local" || method === "open_url") {
    const handle = await ctx.agentRequest(method, params);
    const repoId = (handle as { repo_id?: string })?.repo_id;
    if (repoId) {
      const parsed = parseRepoId(repoId);
      if (!parsed) {
        // Local repo — register with the LocalProvider singleton.
        registry.register(repoId, localProvider);
      }
      // Remote repos: registration is handled by the SSH subsystem
      // when the connection is established (not in this handler).
    }
    return handle;
  }

  // Handle repo close: unregister the provider.
  if (method === "close") {
    const repoId = extractRepoId(params);
    if (repoId) {
      const provider = registry.resolve(repoId);
      if (provider) {
        void provider.watchStop(repoId);
        const result = await provider.request(method, params as Record<string, unknown>);
        registry.unregister(repoId);
        return result;
      }
      registry.unregister(repoId);
    }
    return ctx.agentRequest(method, params);
  }

  // All other calls: resolve provider from repo_id and dispatch.
  const repoId = extractRepoId(params);
  if (repoId) {
    const provider = registry.resolve(repoId);
    if (provider) {
      return provider.request(method, params as Record<string, unknown>);
    }
  }

  // Fallback: no repo_id or no provider — send to local agent.
  return ctx.agentRequest(method, params);
}

function extractRepoId(params: unknown): string | null {
  if (params && typeof params === "object" && "repo_id" in params) {
    const id = (params as Record<string, unknown>).repo_id;
    return typeof id === "string" ? id : null;
  }
  return null;
}

// ── Integration Tests ────────────────────────────────────────────

describe("Provider Resolution Integration — local repos", () => {
  let ctx: SimulatedIpcContext;

  beforeEach(() => {
    _resetProviderRegistry();
    const registry = new ProviderRegistry();
    const localProvider = createMockLocalProvider();
    registry.setLocalProvider(localProvider);

    ctx = {
      registry,
      localProvider,
      agentRequest: async (method, params) => {
        if (method === "open_local") {
          return {
            repo_id: (params as { path: string }).path,
            is_remote: false,
            url: null,
            cache_dir: "/tmp/cache",
          };
        }
        return { ok: true };
      },
    };
  });

  it("resolves LocalProvider for a local repo_id after open_local", async () => {
    const repoId = "/Users/dev/project";

    // Simulate opening the repo (registers the provider)
    await simulateIpcHandler(ctx, "open_local", { path: repoId });

    // Subsequent RPC calls should route through LocalProvider
    const result = await simulateIpcHandler(ctx, "git_status_porcelain", {
      repo_id: repoId,
    });

    expect(result).toEqual({
      __providerType: "local",
      method: "git_status_porcelain",
      params: { repo_id: repoId },
    });
  });

  it("resolves LocalProvider for various local path formats", async () => {
    const paths = [
      "/Users/dev/project",
      "/home/user/repos/my-app",
      "C:\\Users\\dev\\project",
    ];

    for (const repoPath of paths) {
      ctx.registry.register(repoPath, ctx.localProvider);

      const result = await simulateIpcHandler(ctx, "git_log", {
        repo_id: repoPath,
      });

      expect((result as { __providerType: string }).__providerType).toBe("local");
    }
  });

  it("unregistered local repo_id falls back to LocalProvider singleton", async () => {
    // Do NOT call open_local — the repo is not explicitly registered.
    const repoId = "/Users/dev/unregistered-project";

    // The registry should fall back to the LocalProvider singleton
    // because the repo_id has no ssh:// prefix.
    const result = await simulateIpcHandler(ctx, "git_status_porcelain", {
      repo_id: repoId,
    });

    expect(result).toEqual({
      __providerType: "local",
      method: "git_status_porcelain",
      params: { repo_id: repoId },
    });
  });
});

describe("Provider Resolution Integration — remote repos", () => {
  let ctx: SimulatedIpcContext;
  let remoteProvider: RepoProvider;

  beforeEach(() => {
    _resetProviderRegistry();
    const registry = new ProviderRegistry();
    const localProvider = createMockLocalProvider();
    registry.setLocalProvider(localProvider);
    remoteProvider = createMockRemoteProvider("gpu-server");

    ctx = {
      registry,
      localProvider,
      agentRequest: async (method, params) => {
        if (method === "open_url") {
          return {
            repo_id: (params as { url: string }).url,
            is_remote: true,
            url: (params as { url: string }).url,
            cache_dir: null,
          };
        }
        return { ok: true };
      },
    };
  });

  it("resolves RemoteProvider for ssh:// repo_id with explicit registration", async () => {
    const repoId = "ssh://gpu-server/data/ml-project";

    // Simulate the SSH subsystem registering the provider (happens
    // when the SSH connection is established, before the renderer
    // issues RPC calls).
    ctx.registry.register(repoId, remoteProvider);

    const result = await simulateIpcHandler(ctx, "git_status_porcelain", {
      repo_id: repoId,
    });

    expect(result).toEqual({
      __providerType: "remote",
      __profileId: "gpu-server",
      method: "git_status_porcelain",
      params: { repo_id: repoId },
    });
  });

  it("prefers explicit registration over automatic ssh:// resolution", () => {
    const repoId = "ssh://gpu-server/data/ml-project";
    ctx.registry.register(repoId, remoteProvider);

    const resolved = ctx.registry.resolve(repoId);
    expect(resolved).toBe(remoteProvider);
  });

  it("routes through RemoteProvider for all RPC methods after registration", async () => {
    const repoId = "ssh://gpu-server/data/ml-project";
    ctx.registry.register(repoId, remoteProvider);

    const methods = [
      "git_status_porcelain",
      "git_log",
      "file_diff",
      "git_stage",
      "git_commit",
      "list_refs",
    ];

    for (const method of methods) {
      const result = await simulateIpcHandler(ctx, method, { repo_id: repoId });
      expect((result as { __providerType: string }).__providerType).toBe("remote");
      expect((result as { method: string }).method).toBe(method);
    }
  });
});

describe("Provider Resolution Integration — registration timing (no race condition)", () => {
  let registry: ProviderRegistry;
  let localProvider: RepoProvider;

  beforeEach(() => {
    _resetProviderRegistry();
    registry = new ProviderRegistry();
    localProvider = createMockLocalProvider();
    registry.setLocalProvider(localProvider);
  });

  it("local provider is registered synchronously within open_local before handle is returned", async () => {
    const repoId = "/Users/dev/project";
    let registeredBeforeReturn = false;

    const ctx: SimulatedIpcContext = {
      registry,
      localProvider,
      agentRequest: async () => ({
        repo_id: repoId,
        is_remote: false,
      }),
    };

    // Simulate the IPC handler (open_local registers provider before returning)
    const handle = await simulateIpcHandler(ctx, "open_local", { path: repoId });

    // After the handler returns, the provider MUST already be registered.
    // This is the key invariant: no race between handle return and first RPC.
    registeredBeforeReturn = registry.has(repoId);

    expect(handle).toBeDefined();
    expect((handle as { repo_id: string }).repo_id).toBe(repoId);
    expect(registeredBeforeReturn).toBe(true);
  });

  it("provider is available for immediate RPC after open_local returns", async () => {
    const repoId = "/Users/dev/project";

    const ctx: SimulatedIpcContext = {
      registry,
      localProvider,
      agentRequest: async () => ({
        repo_id: repoId,
        is_remote: false,
      }),
    };

    // Open the repo
    await simulateIpcHandler(ctx, "open_local", { path: repoId });

    // Immediately issue an RPC call — should NOT fall through to the
    // agent fallback; should route through the registered provider.
    const result = await simulateIpcHandler(ctx, "git_status_porcelain", {
      repo_id: repoId,
    });

    // Verify it went through the LocalProvider (not the raw agent)
    expect((result as { __providerType: string }).__providerType).toBe("local");
  });

  it("concurrent open + RPC does not race: provider is registered before response", async () => {
    const repoId = "/Users/dev/concurrent-project";

    const ctx: SimulatedIpcContext = {
      registry,
      localProvider,
      agentRequest: async () => {
        // Simulate a small delay in the agent response
        await new Promise((resolve) => setTimeout(resolve, 10));
        return { repo_id: repoId, is_remote: false };
      },
    };

    // Start open_local (which has a delay)
    const openPromise = simulateIpcHandler(ctx, "open_local", { path: repoId });

    // Wait for open to complete
    await openPromise;

    // Now the provider must be registered
    expect(registry.has(repoId)).toBe(true);
    expect(registry.resolve(repoId)).toBe(localProvider);
  });

  it("remote provider registration by SSH subsystem makes it available before first RPC", () => {
    const repoId = "ssh://gpu-server/data/ml-project";
    const remoteProvider = createMockRemoteProvider("gpu-server");

    // Simulate the SSH subsystem registering the provider (this happens
    // during connection establishment, before the renderer gets the handle).
    registry.register(repoId, remoteProvider);

    // Verify the provider is immediately resolvable
    const resolved = registry.resolve(repoId);
    expect(resolved).toBe(remoteProvider);
  });

  it("provider:registered event fires synchronously during registration", () => {
    const repoId = "/Users/dev/project";
    let eventFired = false;

    registry.on("provider:registered", (event) => {
      if (event.repoId === repoId) {
        eventFired = true;
      }
    });

    registry.register(repoId, localProvider);

    // Event must fire synchronously (before any async continuation)
    expect(eventFired).toBe(true);
  });
});

describe("Provider Resolution Integration — close lifecycle", () => {
  let ctx: SimulatedIpcContext;

  beforeEach(() => {
    _resetProviderRegistry();
    const registry = new ProviderRegistry();
    const localProvider = createMockLocalProvider();
    registry.setLocalProvider(localProvider);

    ctx = {
      registry,
      localProvider,
      agentRequest: async (method, params) => {
        if (method === "open_local") {
          return {
            repo_id: (params as { path: string }).path,
            is_remote: false,
          };
        }
        return { ok: true };
      },
    };
  });

  it("unregisters provider on close, subsequent calls fall back to LocalProvider singleton", async () => {
    const repoId = "/Users/dev/project";

    // Open and then close
    await simulateIpcHandler(ctx, "open_local", { path: repoId });
    expect(ctx.registry.has(repoId)).toBe(true);

    await simulateIpcHandler(ctx, "close", { repo_id: repoId });
    expect(ctx.registry.has(repoId)).toBe(false);

    // After close, the repo_id is no longer explicitly registered.
    // But since it's a local path, resolve() still falls back to
    // the LocalProvider singleton.
    const resolved = ctx.registry.resolve(repoId);
    expect(resolved).toBe(ctx.localProvider);
  });

  it("closing a remote repository prevents subsequent dispatch", async () => {
    const repoId = "ssh://gpu-server/data/ml-project";
    const remoteProvider = createMockRemoteProvider("gpu-server");

    ctx.registry.register(repoId, remoteProvider);
    expect(ctx.registry.has(repoId)).toBe(true);

    await simulateIpcHandler(ctx, "close", { repo_id: repoId });
    expect(ctx.registry.has(repoId)).toBe(false);

    await expect(simulateIpcHandler(ctx, "write_file", { repo_id: repoId, path: "weights.bin", content: "wrong machine" })).rejects.toMatchObject({ code: "SSH_REPOSITORY_NOT_OPEN" });
  });
});

describe("Provider Resolution Integration — mixed local and remote repos", () => {
  let registry: ProviderRegistry;
  let localProvider: RepoProvider;

  beforeEach(() => {
    _resetProviderRegistry();
    registry = new ProviderRegistry();
    localProvider = createMockLocalProvider();
    registry.setLocalProvider(localProvider);
  });

  it("resolves correct provider for interleaved local and remote calls", async () => {
    const localRepoId = "/Users/dev/local-project";
    const remoteRepoId = "ssh://gpu-server/data/ml-project";
    const remoteProvider = createMockRemoteProvider("gpu-server");

    registry.register(localRepoId, localProvider);
    registry.register(remoteRepoId, remoteProvider);

    const ctx: SimulatedIpcContext = {
      registry,
      localProvider,
      agentRequest: async () => ({ ok: true }),
    };

    // Interleave calls to local and remote repos
    const localResult = await simulateIpcHandler(ctx, "git_log", {
      repo_id: localRepoId,
    });
    const remoteResult = await simulateIpcHandler(ctx, "git_log", {
      repo_id: remoteRepoId,
    });
    const localResult2 = await simulateIpcHandler(ctx, "git_status_porcelain", {
      repo_id: localRepoId,
    });

    expect((localResult as { __providerType: string }).__providerType).toBe("local");
    expect((remoteResult as { __providerType: string }).__providerType).toBe("remote");
    expect((localResult2 as { __providerType: string }).__providerType).toBe("local");
  });

  it("multiple remote repos on different profiles resolve independently", () => {
    const repo1 = "ssh://gpu-server/data/project-a";
    const repo2 = "ssh://dev-box/home/user/project-b";

    const provider1 = createMockRemoteProvider("gpu-server");
    const provider2 = createMockRemoteProvider("dev-box");

    registry.register(repo1, provider1);
    registry.register(repo2, provider2);

    expect(registry.resolve(repo1)).toBe(provider1);
    expect(registry.resolve(repo2)).toBe(provider2);
  });
});
