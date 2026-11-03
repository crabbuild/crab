/**
 * Unit tests for WorkspaceSessionManager.
 *
 * Validates workspace session persistence: round-trip fidelity,
 * LRU eviction (max 10), flush behavior, corruption handling, and clear.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { WorkspaceSessionManager } from "../src/lib/workspace-session-manager";
import type { WorkspaceSession } from "../src/lib/workspace-session-manager";

// ---------------------------------------------------------------------------
// localStorage mock
// ---------------------------------------------------------------------------

function createLocalStorageMock() {
  const store = new Map<string, string>();
  return {
    getItem: vi.fn((key: string) => store.get(key) ?? null),
    setItem: vi.fn((key: string, value: string) => { store.set(key, value); }),
    removeItem: vi.fn((key: string) => { store.delete(key); }),
    clear: vi.fn(() => { store.clear(); }),
    get length() { return store.size; },
    key: vi.fn((index: number) => [...store.keys()][index] ?? null),
    _store: store,
  };
}

// ---------------------------------------------------------------------------
// Setup / teardown
// ---------------------------------------------------------------------------

let mockStorage: ReturnType<typeof createLocalStorageMock>;

beforeEach(() => {
  vi.useFakeTimers();
  mockStorage = createLocalStorageMock();
  Object.defineProperty(globalThis, "localStorage", { value: mockStorage, writable: true });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("WorkspaceSessionManager", () => {
  describe("save → restore round-trip", () => {
    it("preserves all fields through save and restore from cache", () => {
      const mgr = new WorkspaceSessionManager();
      const session: Partial<WorkspaceSession> = {
        activeRepoRelativePath: "packages/frontend",
        tabBarOrder: ["packages/frontend", "packages/backend", "packages/shared"],
        terminalPanelOpen: true,
        sidebarWidth: 300,
      };

      mgr.save("/Users/dev/my-workspace", session);
      const restored = mgr.restore("/Users/dev/my-workspace");

      expect(restored).not.toBeNull();
      expect(restored!.workspaceRoot).toBe("/Users/dev/my-workspace");
      expect(restored!.activeRepoRelativePath).toBe("packages/frontend");
      expect(restored!.tabBarOrder).toEqual(["packages/frontend", "packages/backend", "packages/shared"]);
      expect(restored!.terminalPanelOpen).toBe(true);
      expect(restored!.sidebarWidth).toBe(300);
      expect(restored!.savedAt).toBeTypeOf("number");
    });

    it("preserves all fields through save, flush, and cold restore from localStorage", () => {
      const mgr = new WorkspaceSessionManager();
      const session: Partial<WorkspaceSession> = {
        activeRepoRelativePath: "apps/web",
        tabBarOrder: ["apps/web", "libs/core"],
        terminalPanelOpen: false,
        sidebarWidth: 240,
      };

      mgr.save("/Users/dev/monorepo", session);
      mgr.flush();

      // Fresh manager simulates cold start.
      const mgr2 = new WorkspaceSessionManager();
      const restored = mgr2.restore("/Users/dev/monorepo");

      expect(restored).not.toBeNull();
      expect(restored!.workspaceRoot).toBe("/Users/dev/monorepo");
      expect(restored!.activeRepoRelativePath).toBe("apps/web");
      expect(restored!.tabBarOrder).toEqual(["apps/web", "libs/core"]);
      expect(restored!.terminalPanelOpen).toBe(false);
      expect(restored!.sidebarWidth).toBe(240);
    });

    it("returns null for a workspace with no saved session", () => {
      const mgr = new WorkspaceSessionManager();
      const result = mgr.restore("/Users/dev/unknown-workspace");
      expect(result).toBeNull();
    });

    it("merges partial updates into existing session", () => {
      const mgr = new WorkspaceSessionManager();

      mgr.save("/Users/dev/ws", {
        activeRepoRelativePath: "repo-a",
        tabBarOrder: ["repo-a", "repo-b"],
        terminalPanelOpen: true,
        sidebarWidth: 280,
      });

      // Update only the active repo.
      mgr.save("/Users/dev/ws", { activeRepoRelativePath: "repo-b" });

      const restored = mgr.restore("/Users/dev/ws");
      expect(restored!.activeRepoRelativePath).toBe("repo-b");
      // Other fields should be preserved from the first save.
      expect(restored!.tabBarOrder).toEqual(["repo-a", "repo-b"]);
      expect(restored!.terminalPanelOpen).toBe(true);
      expect(restored!.sidebarWidth).toBe(280);
    });
  });

  describe("LRU eviction at cap (max 10)", () => {
    it("evicts the oldest workspace session when the 11th is flushed", () => {
      const mgr = new WorkspaceSessionManager();

      // Create 11 workspace sessions with increasing timestamps.
      for (let i = 0; i < 11; i++) {
        vi.setSystemTime(new Date(1000 + i));
        mgr.save(`/workspaces/ws-${i}`, { activeRepoRelativePath: `repo-${i}` });
      }

      mgr.flush();

      // The index should have at most 10 entries.
      const indexRaw = mockStorage.getItem("crab.wsSessionIndex.v1");
      expect(indexRaw).not.toBeNull();
      const index = JSON.parse(indexRaw!);
      expect(index.length).toBeLessThanOrEqual(10);

      // The oldest (ws-0) should be evicted.
      const mgr2 = new WorkspaceSessionManager();
      const evicted = mgr2.restore("/workspaces/ws-0");
      expect(evicted).toBeNull();

      // The newest (ws-10) should still be available.
      const newest = mgr2.restore("/workspaces/ws-10");
      expect(newest).not.toBeNull();
      expect(newest!.activeRepoRelativePath).toBe("repo-10");
    });
  });

  describe("flush() writes to localStorage correctly", () => {
    it("writes dirty sessions with correct key prefix", () => {
      const mgr = new WorkspaceSessionManager();
      mgr.save("/Users/dev/project", { activeRepoRelativePath: "src" });
      mgr.flush();

      const keys = [...mockStorage._store.keys()];
      const sessionKeys = keys.filter((k) => k.startsWith("crab.wsSession.v1."));
      expect(sessionKeys.length).toBe(1);

      const stored = JSON.parse(mockStorage._store.get(sessionKeys[0]!)!);
      expect(stored.workspaceRoot).toBe("/Users/dev/project");
      expect(stored.activeRepoRelativePath).toBe("src");
    });

    it("updates the workspace session index on flush", () => {
      const mgr = new WorkspaceSessionManager();
      mgr.save("/ws/alpha", { activeRepoRelativePath: "a" });
      mgr.save("/ws/beta", { activeRepoRelativePath: "b" });
      mgr.flush();

      const indexRaw = mockStorage.getItem("crab.wsSessionIndex.v1");
      expect(indexRaw).not.toBeNull();
      const index = JSON.parse(indexRaw!);
      expect(index.length).toBe(2);

      const roots = index.map((e: { workspaceRoot: string }) => e.workspaceRoot).sort();
      expect(roots).toEqual(["/ws/alpha", "/ws/beta"]);
    });

    it("does not write when there are no dirty sessions", () => {
      const mgr = new WorkspaceSessionManager();
      mgr.flush();
      expect(mockStorage.setItem).not.toHaveBeenCalled();
    });
  });

  describe("corrupted JSON in localStorage", () => {
    it("returns null and logs warning for invalid JSON", () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      const mgr = new WorkspaceSessionManager();

      mgr.save("/ws/corrupted", { activeRepoRelativePath: "x" });
      mgr.flush();

      const keys = [...mockStorage._store.keys()].filter((k) =>
        k.startsWith("crab.wsSession.v1."),
      );
      expect(keys.length).toBe(1);

      // Corrupt the stored value.
      mockStorage._store.set(keys[0]!, "not valid json {{{");

      const mgr2 = new WorkspaceSessionManager();
      const result = mgr2.restore("/ws/corrupted");

      expect(result).toBeNull();
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining("[WorkspaceSessionManager]"),
        expect.anything(),
      );

      warnSpy.mockRestore();
    });

    it("returns null for data missing required fields", () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      const mgr = new WorkspaceSessionManager();

      mgr.save("/ws/incomplete", { activeRepoRelativePath: "x" });
      mgr.flush();

      const keys = [...mockStorage._store.keys()].filter((k) =>
        k.startsWith("crab.wsSession.v1."),
      );

      // Write valid JSON but missing required fields (no tabBarOrder array).
      mockStorage._store.set(keys[0]!, JSON.stringify({ foo: "bar" }));

      const mgr2 = new WorkspaceSessionManager();
      const result = mgr2.restore("/ws/incomplete");

      expect(result).toBeNull();
      expect(warnSpy).toHaveBeenCalled();

      warnSpy.mockRestore();
    });
  });

  describe("clear() removes all workspace session keys", () => {
    it("removes all workspace session data from localStorage and resets state", () => {
      const mgr = new WorkspaceSessionManager();

      mgr.save("/ws/one", { activeRepoRelativePath: "a" });
      mgr.save("/ws/two", { activeRepoRelativePath: "b" });
      mgr.save("/ws/three", { activeRepoRelativePath: "c" });
      mgr.flush();

      const keysBefore = [...mockStorage._store.keys()];
      expect(keysBefore.length).toBeGreaterThan(0);

      mgr.clear();

      const keysAfter = [...mockStorage._store.keys()].filter(
        (k) => k.startsWith("crab.wsSession.v1.") || k === "crab.wsSessionIndex.v1",
      );
      expect(keysAfter.length).toBe(0);

      // Restoring any previously stored session should return null.
      const result = mgr.restore("/ws/one");
      expect(result).toBeNull();
    });
  });

  describe("debounce: save schedules a single flush", () => {
    it("batches multiple saves into a single flush after debounce period", () => {
      const mgr = new WorkspaceSessionManager();

      mgr.save("/ws/active", { activeRepoRelativePath: "repo-1" });
      mgr.save("/ws/active", { activeRepoRelativePath: "repo-2" });
      mgr.save("/ws/active", { activeRepoRelativePath: "repo-3" });

      // No flush should have happened yet.
      const sessionWritesBefore = mockStorage.setItem.mock.calls.filter(
        ([key]: [string, string]) => key.startsWith("crab.wsSession.v1."),
      );
      expect(sessionWritesBefore.length).toBe(0);

      // Advance past debounce period.
      vi.advanceTimersByTime(250);

      const sessionWrites = mockStorage.setItem.mock.calls.filter(
        ([key]: [string, string]) => key.startsWith("crab.wsSession.v1."),
      );
      expect(sessionWrites.length).toBe(1);

      // The flushed value should reflect the last save.
      const stored = JSON.parse(sessionWrites[0]![1]);
      expect(stored.activeRepoRelativePath).toBe("repo-3");
    });
  });

  describe("legacy key migration", () => {
    it("migrates legacy activeRepo and tabBar keys into a new WorkspaceSession", () => {
      // Seed legacy keys.
      mockStorage._store.set("crab.workspaceActiveRepo.v1", "packages/frontend");
      mockStorage._store.set("crab.workspaceTabBar.v1", JSON.stringify(["packages/frontend", "packages/backend"]));

      const mgr = new WorkspaceSessionManager();
      const restored = mgr.restore("/Users/dev/my-workspace");

      expect(restored).not.toBeNull();
      expect(restored!.workspaceRoot).toBe("/Users/dev/my-workspace");
      expect(restored!.activeRepoRelativePath).toBe("packages/frontend");
      expect(restored!.tabBarOrder).toEqual(["packages/frontend", "packages/backend"]);
      expect(restored!.terminalPanelOpen).toBe(false);
      expect(restored!.sidebarWidth).toBe(260);

      // Legacy keys should be removed after migration.
      expect(mockStorage._store.has("crab.workspaceActiveRepo.v1")).toBe(false);
      expect(mockStorage._store.has("crab.workspaceTabBar.v1")).toBe(false);
    });

    it("migrates only activeRepo when tabBar key is missing", () => {
      mockStorage._store.set("crab.workspaceActiveRepo.v1", "my-repo");

      const mgr = new WorkspaceSessionManager();
      const restored = mgr.restore("/ws/solo");

      expect(restored).not.toBeNull();
      expect(restored!.activeRepoRelativePath).toBe("my-repo");
      expect(restored!.tabBarOrder).toEqual([]);

      expect(mockStorage._store.has("crab.workspaceActiveRepo.v1")).toBe(false);
    });

    it("migrates only tabBar when activeRepo key is missing", () => {
      mockStorage._store.set("crab.workspaceTabBar.v1", JSON.stringify(["repo-a", "repo-b"]));

      const mgr = new WorkspaceSessionManager();
      const restored = mgr.restore("/ws/tabs-only");

      expect(restored).not.toBeNull();
      expect(restored!.activeRepoRelativePath).toBeNull();
      expect(restored!.tabBarOrder).toEqual(["repo-a", "repo-b"]);

      expect(mockStorage._store.has("crab.workspaceTabBar.v1")).toBe(false);
    });

    it("is idempotent — does not overwrite an existing new-format session", () => {
      const mgr = new WorkspaceSessionManager();

      // Save a new-format session first.
      mgr.save("/ws/existing", {
        activeRepoRelativePath: "new-format-repo",
        tabBarOrder: ["new-format-repo"],
      });
      mgr.flush();

      // Now add legacy keys (simulating leftover data).
      mockStorage._store.set("crab.workspaceActiveRepo.v1", "legacy-repo");
      mockStorage._store.set("crab.workspaceTabBar.v1", JSON.stringify(["legacy-repo"]));

      // Fresh manager — restore should return the existing new-format session.
      const mgr2 = new WorkspaceSessionManager();
      const restored = mgr2.restore("/ws/existing");

      expect(restored).not.toBeNull();
      expect(restored!.activeRepoRelativePath).toBe("new-format-repo");
      expect(restored!.tabBarOrder).toEqual(["new-format-repo"]);

      // Legacy keys should NOT have been deleted (migration didn't run).
      expect(mockStorage._store.has("crab.workspaceActiveRepo.v1")).toBe(true);
    });

    it("returns null when no legacy keys exist and no new-format session exists", () => {
      const mgr = new WorkspaceSessionManager();
      const result = mgr.restore("/ws/empty");
      expect(result).toBeNull();
    });

    it("handles corrupted legacy tabBar JSON gracefully", () => {
      mockStorage._store.set("crab.workspaceActiveRepo.v1", "some-repo");
      mockStorage._store.set("crab.workspaceTabBar.v1", "not valid json {{{");

      const mgr = new WorkspaceSessionManager();
      const restored = mgr.restore("/ws/corrupt-tabs");

      expect(restored).not.toBeNull();
      expect(restored!.activeRepoRelativePath).toBe("some-repo");
      expect(restored!.tabBarOrder).toEqual([]);

      // Legacy keys should still be cleaned up.
      expect(mockStorage._store.has("crab.workspaceActiveRepo.v1")).toBe(false);
      expect(mockStorage._store.has("crab.workspaceTabBar.v1")).toBe(false);
    });

    it("persists the migrated session to localStorage", () => {
      mockStorage._store.set("crab.workspaceActiveRepo.v1", "migrated-repo");
      mockStorage._store.set("crab.workspaceTabBar.v1", JSON.stringify(["migrated-repo"]));

      const mgr = new WorkspaceSessionManager();
      mgr.restore("/ws/persisted");

      // The migrated session should be in localStorage under the new key format.
      const sessionKeys = [...mockStorage._store.keys()].filter((k) =>
        k.startsWith("crab.wsSession.v1."),
      );
      expect(sessionKeys.length).toBe(1);

      const stored = JSON.parse(mockStorage._store.get(sessionKeys[0]!)!);
      expect(stored.workspaceRoot).toBe("/ws/persisted");
      expect(stored.activeRepoRelativePath).toBe("migrated-repo");
      expect(stored.tabBarOrder).toEqual(["migrated-repo"]);
    });
  });
});
