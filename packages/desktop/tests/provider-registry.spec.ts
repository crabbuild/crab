/**
 * Unit tests for the ProviderRegistry module.
 *
 * Tests registration, unregistration, resolution logic (explicit,
 * ssh:// fallback, local fallback), event emission, and singleton
 * behavior.
 */

import { describe, it, expect, beforeEach } from "vitest";
import {
  ProviderRegistry,
  getProviderRegistry,
  _resetProviderRegistry,
} from "../electron/provider-registry";
import type { RepoProvider } from "../electron/repo-provider";
import type { ProviderRegisteredEvent, ProviderUnregisteredEvent } from "../electron/provider-registry";

// ─── Mock Provider ───────────────────────────────────────────────────────────

function createMockProvider(label = "mock"): RepoProvider {
  return {
    request: async () => ({ label }),
    watchStart: async () => {},
    watchStop: async () => {},
    fsCreate: async () => ({ path: "", kind: "file" }),
    fsRename: async () => ({ oldPath: "", newPath: "" }),
    fsDelete: async () => ({ deleted: "", trash: false }),
    fsWrite: async () => ({ path: "", written: 0 }),
    fsRead: async () => ({ exists: false, content: null }),
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

// ─── Tests ───────────────────────────────────────────────────────────────────

describe("ProviderRegistry — registration", () => {
  let registry: ProviderRegistry;

  beforeEach(() => {
    registry = new ProviderRegistry();
  });

  it("registers a provider for a repo_id", () => {
    const provider = createMockProvider();
    registry.register("repo-1", provider);
    expect(registry.has("repo-1")).toBe(true);
  });

  it("overwrites existing registration for the same repo_id", () => {
    const provider1 = createMockProvider("first");
    const provider2 = createMockProvider("second");

    registry.register("repo-1", provider1);
    registry.register("repo-1", provider2);

    const resolved = registry.resolve("repo-1");
    expect(resolved).toBe(provider2);
  });

  it("unregisters a provider", () => {
    const provider = createMockProvider();
    registry.register("repo-1", provider);
    registry.unregister("repo-1");
    expect(registry.has("repo-1")).toBe(false);
  });

  it("unregister is a no-op for unknown repo_ids", () => {
    // Should not throw
    registry.unregister("nonexistent");
    expect(registry.has("nonexistent")).toBe(false);
  });

  it("lists all registered repo_ids", () => {
    registry.register("repo-a", createMockProvider());
    registry.register("repo-b", createMockProvider());
    registry.register("repo-c", createMockProvider());

    const ids = registry.registeredIds();
    expect(ids).toHaveLength(3);
    expect(ids).toContain("repo-a");
    expect(ids).toContain("repo-b");
    expect(ids).toContain("repo-c");
  });

  it("clear removes all registrations", () => {
    registry.register("repo-a", createMockProvider());
    registry.register("repo-b", createMockProvider());
    registry.clear();

    expect(registry.registeredIds()).toHaveLength(0);
    expect(registry.has("repo-a")).toBe(false);
    expect(registry.has("repo-b")).toBe(false);
  });
});

describe("ProviderRegistry — resolution", () => {
  let registry: ProviderRegistry;
  let localProvider: RepoProvider;

  beforeEach(() => {
    registry = new ProviderRegistry();
    localProvider = createMockProvider("local");
    registry.setLocalProvider(localProvider);
  });

  it("resolves explicitly registered provider", () => {
    const remoteProvider = createMockProvider("remote");
    registry.register("ssh://gpu-server/data/repo", remoteProvider);

    const resolved = registry.resolve("ssh://gpu-server/data/repo");
    expect(resolved).toBe(remoteProvider);
  });

  it("falls back to LocalProvider for local repo_ids without registration", () => {
    const resolved = registry.resolve("/home/user/my-project");
    expect(resolved).toBe(localProvider);
  });

  it("rejects unregistered SSH repositories before a caller can use local handling", () => {
    expect(() => registry.resolve("ssh://unknown-server/repo")).toThrow("SSH repository is not open");
  });

  it("returns null when LocalProvider is not set and repo_id is local", () => {
    const emptyRegistry = new ProviderRegistry();
    const resolved = emptyRegistry.resolve("/home/user/repo");
    expect(resolved).toBeNull();
  });

  it("prefers explicit registration over automatic resolution", () => {
    const explicitProvider = createMockProvider("explicit");
    registry.register("ssh://server/repo", explicitProvider);

    // Even though this is an ssh:// repo_id, the explicit registration wins
    const resolved = registry.resolve("ssh://server/repo");
    expect(resolved).toBe(explicitProvider);
  });

  it("falls back to LocalProvider for non-ssh:// prefixed repo_ids", () => {
    // Various local path formats
    expect(registry.resolve("/Users/dev/project")).toBe(localProvider);
    expect(registry.resolve("C:\\Users\\dev\\project")).toBe(localProvider);
    expect(registry.resolve("relative/path")).toBe(localProvider);
  });
});

describe("ProviderRegistry — events", () => {
  let registry: ProviderRegistry;

  beforeEach(() => {
    registry = new ProviderRegistry();
  });

  it("emits provider:registered on register", () => {
    const events: ProviderRegisteredEvent[] = [];
    registry.on("provider:registered", (e) => events.push(e));

    const provider = createMockProvider();
    registry.register("repo-1", provider);

    expect(events).toHaveLength(1);
    expect(events[0].repoId).toBe("repo-1");
    expect(events[0].provider).toBe(provider);
  });

  it("emits provider:unregistered on unregister", () => {
    const events: ProviderUnregisteredEvent[] = [];
    registry.on("provider:unregistered", (e) => events.push(e));

    registry.register("repo-1", createMockProvider());
    registry.unregister("repo-1");

    expect(events).toHaveLength(1);
    expect(events[0].repoId).toBe("repo-1");
  });

  it("does not emit provider:unregistered for unknown repo_ids", () => {
    const events: ProviderUnregisteredEvent[] = [];
    registry.on("provider:unregistered", (e) => events.push(e));

    registry.unregister("nonexistent");

    expect(events).toHaveLength(0);
  });

  it("emits provider:unregistered for each entry on clear", () => {
    const events: ProviderUnregisteredEvent[] = [];
    registry.on("provider:unregistered", (e) => events.push(e));

    registry.register("repo-a", createMockProvider());
    registry.register("repo-b", createMockProvider());
    registry.clear();

    expect(events).toHaveLength(2);
    const repoIds = events.map((e) => e.repoId);
    expect(repoIds).toContain("repo-a");
    expect(repoIds).toContain("repo-b");
  });
});

describe("ProviderRegistry — singleton", () => {
  beforeEach(() => {
    _resetProviderRegistry();
  });

  it("returns the same instance on repeated calls", () => {
    const a = getProviderRegistry();
    const b = getProviderRegistry();
    expect(a).toBe(b);
  });

  it("returns a fresh instance after reset", () => {
    const a = getProviderRegistry();
    _resetProviderRegistry();
    const b = getProviderRegistry();
    expect(a).not.toBe(b);
  });
});
