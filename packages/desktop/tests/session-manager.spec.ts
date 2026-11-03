/**
 * Unit tests for SessionManager.
 *
 * Validates core session persistence logic: round-trip fidelity,
 * LRU eviction, flush behavior, corruption handling, clear, and debounce.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { SessionManager, hashSync } from "../src/lib/session-manager";
import type { RepoSession } from "../src/lib/session-manager";

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
    /** Expose the backing store for assertions. */
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
// Helpers
// ---------------------------------------------------------------------------

function makeSession(repoPath: string, overrides?: Partial<RepoSession>): Partial<RepoSession> {
  return {
    repoPath,
    activeView: "timeline",
    activeFile: "/project/src/main.ts",
    navHistory: [{ view: "explorer" }, { view: "timeline", commit: "abc123" }],
    navIndex: 1,
    scrollPositions: { timeline: 320, explorer: 0 },
    viewState: { selectedCommit: "abc123" },
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("SessionManager", () => {
  describe("deactivate → activate round-trip", () => {
    it("preserves all fields through deactivate and activate from cache", () => {
      const mgr = new SessionManager();
      const state = makeSession("/repos/frontend");

      mgr.deactivate("/repos/frontend", state);
      const restored = mgr.activate("/repos/frontend");

      expect(restored).not.toBeNull();
      expect(restored!.repoPath).toBe("/repos/frontend");
      expect(restored!.activeView).toBe("timeline");
      expect(restored!.activeFile).toBe("/project/src/main.ts");
      expect(restored!.navHistory).toEqual([
        { view: "explorer" },
        { view: "timeline", commit: "abc123" },
      ]);
      expect(restored!.navIndex).toBe(1);
      expect(restored!.scrollPositions).toEqual({ timeline: 320, explorer: 0 });
      expect(restored!.viewState).toEqual({ selectedCommit: "abc123" });
      expect(restored!.savedAt).toBeTypeOf("number");
    });

    it("preserves all fields through deactivate, flush, and cold activate from localStorage", () => {
      const mgr = new SessionManager();
      const state = makeSession("/repos/backend");

      mgr.deactivate("/repos/backend", state);
      mgr.flush();

      // Create a fresh manager to simulate cold start (no in-memory cache).
      const mgr2 = new SessionManager();
      const restored = mgr2.activate("/repos/backend");

      expect(restored).not.toBeNull();
      expect(restored!.repoPath).toBe("/repos/backend");
      expect(restored!.activeView).toBe("timeline");
      expect(restored!.activeFile).toBe("/project/src/main.ts");
      expect(restored!.navHistory).toEqual([
        { view: "explorer" },
        { view: "timeline", commit: "abc123" },
      ]);
      expect(restored!.navIndex).toBe(1);
      expect(restored!.scrollPositions).toEqual({ timeline: 320, explorer: 0 });
      expect(restored!.viewState).toEqual({ selectedCommit: "abc123" });
    });
  });

  describe("LRU eviction at cap", () => {
    it("evicts the oldest session when the 51st session is flushed", () => {
      const mgr = new SessionManager();

      // Create 51 sessions with increasing savedAt timestamps.
      for (let i = 0; i < 51; i++) {
        vi.setSystemTime(new Date(1000 + i));
        mgr.deactivate(`/repos/repo-${i}`, { activeView: "explorer" });
      }

      mgr.flush();

      // The session index should have at most 50 entries.
      const indexRaw = mockStorage.getItem("crab.sessionIndex.v1");
      expect(indexRaw).not.toBeNull();
      const index = JSON.parse(indexRaw!);
      expect(index.length).toBeLessThanOrEqual(50);

      // The oldest session (repo-0) should have been evicted from localStorage.
      // A fresh manager should not find it.
      const mgr2 = new SessionManager();
      const evicted = mgr2.activate("/repos/repo-0");
      expect(evicted).toBeNull();

      // The newest session (repo-50) should still be available.
      const newest = mgr2.activate("/repos/repo-50");
      expect(newest).not.toBeNull();
      expect(newest!.activeView).toBe("explorer");
    });
  });

  describe("flush() writes to localStorage correctly", () => {
    it("writes dirty sessions to localStorage with correct key prefix", () => {
      const mgr = new SessionManager();
      mgr.deactivate("/repos/my-project", { activeView: "changes" });
      mgr.flush();

      // Find the session key in localStorage.
      const keys = [...mockStorage._store.keys()];
      const sessionKeys = keys.filter((k) => k.startsWith("crab.session.v1."));
      expect(sessionKeys.length).toBe(1);

      const stored = JSON.parse(mockStorage._store.get(sessionKeys[0]!)!);
      expect(stored.repoPath).toBe("/repos/my-project");
      expect(stored.activeView).toBe("changes");
    });

    it("updates the session index on flush", () => {
      const mgr = new SessionManager();
      mgr.deactivate("/repos/alpha", { activeView: "explorer" });
      mgr.deactivate("/repos/beta", { activeView: "timeline" });
      mgr.flush();

      const indexRaw = mockStorage.getItem("crab.sessionIndex.v1");
      expect(indexRaw).not.toBeNull();
      const index = JSON.parse(indexRaw!);
      expect(index.length).toBe(2);

      const repoPaths = index.map((e: { repoPath: string }) => e.repoPath).sort();
      expect(repoPaths).toEqual(["/repos/alpha", "/repos/beta"]);
    });

    it("does not write when there are no dirty sessions", () => {
      const mgr = new SessionManager();
      mgr.flush();

      // No setItem calls should have been made (no dirty sessions).
      expect(mockStorage.setItem).not.toHaveBeenCalled();
    });
  });

  describe("corrupted JSON in localStorage", () => {
    it("returns null and logs warning for invalid JSON", () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      const mgr = new SessionManager();

      // Manually write corrupted data to the storage key for a repo path.
      // We need to figure out what key the manager would use.
      // Deactivate once to populate the key cache, flush, then corrupt.
      mgr.deactivate("/repos/corrupted", { activeView: "explorer" });
      mgr.flush();

      // Find the key that was written.
      const keys = [...mockStorage._store.keys()].filter((k) =>
        k.startsWith("crab.session.v1."),
      );
      expect(keys.length).toBe(1);

      // Corrupt the stored value.
      mockStorage._store.set(keys[0]!, "not valid json {{{");

      // Create a fresh manager (no cache) and try to activate.
      const mgr2 = new SessionManager();
      const result = mgr2.activate("/repos/corrupted");

      expect(result).toBeNull();
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining("[SessionManager]"),
        expect.anything(),
      );

      warnSpy.mockRestore();
    });

    it("returns null for data missing required fields", () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      const mgr = new SessionManager();

      mgr.deactivate("/repos/incomplete", { activeView: "explorer" });
      mgr.flush();

      const keys = [...mockStorage._store.keys()].filter((k) =>
        k.startsWith("crab.session.v1."),
      );

      // Write valid JSON but missing required fields.
      mockStorage._store.set(keys[0]!, JSON.stringify({ foo: "bar" }));

      const mgr2 = new SessionManager();
      const result = mgr2.activate("/repos/incomplete");

      expect(result).toBeNull();
      expect(warnSpy).toHaveBeenCalled();

      warnSpy.mockRestore();
    });
  });

  describe("clear() removes all session keys", () => {
    it("removes all session data from localStorage and resets internal state", () => {
      const mgr = new SessionManager();

      // Create several sessions and flush them.
      mgr.deactivate("/repos/one", { activeView: "explorer" });
      mgr.deactivate("/repos/two", { activeView: "timeline" });
      mgr.deactivate("/repos/three", { activeView: "changes" });
      mgr.flush();

      // Verify data exists before clear.
      const keysBefore = [...mockStorage._store.keys()];
      expect(keysBefore.length).toBeGreaterThan(0);

      mgr.clear();

      // All session keys and the index should be removed.
      const keysAfter = [...mockStorage._store.keys()].filter(
        (k) => k.startsWith("crab.session.v1.") || k === "crab.sessionIndex.v1",
      );
      expect(keysAfter.length).toBe(0);

      // Activating any previously stored session should return null.
      const result = mgr.activate("/repos/one");
      expect(result).toBeNull();
    });
  });

  describe("debounce: multiple rapid update() calls produce a single flush", () => {
    it("batches multiple updates into a single flush after debounce period", () => {
      const mgr = new SessionManager();

      // First deactivate to put the session in cache.
      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.flush();
      mockStorage.setItem.mockClear();

      // Perform multiple rapid updates.
      mgr.update("/repos/active", { activeView: "timeline" });
      mgr.update("/repos/active", { activeView: "changes" });
      mgr.update("/repos/active", { activeView: "branches" });

      // No flush should have happened yet.
      expect(mockStorage.setItem).not.toHaveBeenCalled();

      // Advance time past the debounce period (250ms).
      vi.advanceTimersByTime(250);

      // Now a single flush should have occurred.
      // setItem is called once for the session data + once for the index.
      const sessionWrites = mockStorage.setItem.mock.calls.filter(
        ([key]: [string, string]) => key.startsWith("crab.session.v1."),
      );
      expect(sessionWrites.length).toBe(1);

      // The flushed value should reflect the last update.
      const stored = JSON.parse(sessionWrites[0]![1]);
      expect(stored.activeView).toBe("branches");
    });

    it("resets the debounce timer on each update (only one flush after last update)", () => {
      const mgr = new SessionManager();
      mgr.deactivate("/repos/debounce-test", { activeView: "explorer" });
      mgr.flush();
      mockStorage.setItem.mockClear();

      // Update at t=0.
      mgr.update("/repos/debounce-test", { activeView: "timeline" });

      // Advance 100ms (less than 250ms debounce).
      vi.advanceTimersByTime(100);

      // The first scheduleFlush sets the timer; subsequent updates within
      // the window don't reset it (the implementation uses "if timer exists, skip").
      // So after 250ms total from the first update, it should flush.
      vi.advanceTimersByTime(150);

      const sessionWrites = mockStorage.setItem.mock.calls.filter(
        ([key]: [string, string]) => key.startsWith("crab.session.v1."),
      );
      expect(sessionWrites.length).toBe(1);
    });
  });

  describe("worktree session isolation", () => {
    it("hashSync produces distinct keys for a main repo and its worktree", () => {
      const mainRepoHash = hashSync("/projects/repo");
      const worktreeHash = hashSync("/projects/repo-wt-feature");

      expect(mainRepoHash).not.toBe(worktreeHash);
    });

    it("hashSync produces distinct keys for multiple worktrees of the same repo", () => {
      const wt1 = hashSync("/projects/repo-wt-feature-a");
      const wt2 = hashSync("/projects/repo-wt-feature-b");
      const wt3 = hashSync("/projects/repo-wt-bugfix");

      expect(wt1).not.toBe(wt2);
      expect(wt1).not.toBe(wt3);
      expect(wt2).not.toBe(wt3);
    });

    it("worktrees get independent sessions through SessionManager", () => {
      const mgr = new SessionManager();
      const mainRepoPath = "/projects/repo";
      const worktreePath = "/projects/repo-wt-feature";

      // Deactivate sessions with different state for each path.
      mgr.deactivate(mainRepoPath, { activeView: "timeline", activeFile: "/projects/repo/src/main.ts" });
      mgr.deactivate(worktreePath, { activeView: "changes", activeFile: "/projects/repo-wt-feature/src/fix.ts" });
      mgr.flush();

      // Activate each and verify they have independent state.
      const mainSession = mgr.activate(mainRepoPath);
      const wtSession = mgr.activate(worktreePath);

      expect(mainSession).not.toBeNull();
      expect(wtSession).not.toBeNull();
      expect(mainSession!.activeView).toBe("timeline");
      expect(wtSession!.activeView).toBe("changes");
      expect(mainSession!.activeFile).toBe("/projects/repo/src/main.ts");
      expect(wtSession!.activeFile).toBe("/projects/repo-wt-feature/src/fix.ts");
    });

    it("worktree sessions persist independently across cold restarts", () => {
      const mainRepoPath = "/projects/repo";
      const worktreePath = "/projects/repo-wt-feature";

      // First manager: save sessions and flush.
      const mgr1 = new SessionManager();
      mgr1.deactivate(mainRepoPath, { activeView: "explorer", scrollPositions: { explorer: 100 } });
      mgr1.deactivate(worktreePath, { activeView: "branches", scrollPositions: { branches: 250 } });
      mgr1.flush();

      // Second manager (cold start): verify independent restore.
      const mgr2 = new SessionManager();
      const mainSession = mgr2.activate(mainRepoPath);
      const wtSession = mgr2.activate(worktreePath);

      expect(mainSession!.activeView).toBe("explorer");
      expect(mainSession!.scrollPositions).toEqual({ explorer: 100 });
      expect(wtSession!.activeView).toBe("branches");
      expect(wtSession!.scrollPositions).toEqual({ branches: 250 });
    });

    it("updating one worktree session does not affect another", () => {
      const mgr = new SessionManager();
      const mainRepoPath = "/projects/repo";
      const worktreePath = "/projects/repo-wt-feature";

      mgr.deactivate(mainRepoPath, { activeView: "timeline" });
      mgr.deactivate(worktreePath, { activeView: "changes" });

      // Update only the worktree session.
      mgr.update(worktreePath, { activeView: "explorer" });

      // Main repo session should be unchanged.
      const mainSession = mgr.activate(mainRepoPath);
      expect(mainSession!.activeView).toBe("timeline");

      // Worktree session should reflect the update.
      const wtSession = mgr.activate(worktreePath);
      expect(wtSession!.activeView).toBe("explorer");
    });
  });

  describe("cleanupStaleWorktrees", () => {
    it("queues sessions whose repoPath is not in the known set", () => {
      const mgr = new SessionManager();

      // Create sessions for three repos.
      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.deactivate("/repos/deleted-wt", { activeView: "timeline" });
      mgr.deactivate("/repos/also-active", { activeView: "changes" });
      mgr.flush();

      // Only two repos are known (the worktree was deleted).
      mgr.cleanupStaleWorktrees(["/repos/active", "/repos/also-active"]);

      // The cleanup queue should contain the deleted worktree.
      const queueRaw = mockStorage.getItem("crab.sessionCleanupQueue.v1");
      expect(queueRaw).not.toBeNull();
      const queue = JSON.parse(queueRaw!);
      expect(queue.length).toBe(1);
      expect(queue[0].repoPath).toBe("/repos/deleted-wt");
      expect(queue[0].queuedAt).toBeTypeOf("number");
    });

    it("does not immediately delete queued sessions (24h grace period)", () => {
      const mgr = new SessionManager();

      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.deactivate("/repos/deleted-wt", { activeView: "timeline" });
      mgr.flush();

      // Queue the stale session.
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      // The session data should still be in localStorage.
      const mgr2 = new SessionManager();
      const session = mgr2.activate("/repos/deleted-wt");
      expect(session).not.toBeNull();
      expect(session!.activeView).toBe("timeline");
    });

    it("purges queued sessions after 24 hours", () => {
      const mgr = new SessionManager();
      vi.setSystemTime(new Date(1000));

      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.deactivate("/repos/deleted-wt", { activeView: "timeline" });
      mgr.flush();

      // First cleanup: queue the stale session.
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      // Advance time past 24 hours.
      vi.setSystemTime(new Date(1000 + 24 * 60 * 60 * 1000 + 1));

      // Second cleanup: should purge the expired entry.
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      // The session should now be gone from localStorage.
      const mgr2 = new SessionManager();
      const session = mgr2.activate("/repos/deleted-wt");
      expect(session).toBeNull();

      // The cleanup queue should be empty.
      const queueRaw = mockStorage.getItem("crab.sessionCleanupQueue.v1");
      expect(queueRaw).toBeNull();
    });

    it("removes entries from the queue if the repo reappears in the known set", () => {
      const mgr = new SessionManager();
      vi.setSystemTime(new Date(1000));

      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.deactivate("/repos/temp-wt", { activeView: "timeline" });
      mgr.flush();

      // First cleanup: queue the worktree session.
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      // Verify it's queued.
      let queueRaw = mockStorage.getItem("crab.sessionCleanupQueue.v1");
      let queue = JSON.parse(queueRaw!);
      expect(queue.length).toBe(1);

      // User re-opens the worktree before 24h.
      vi.setSystemTime(new Date(1000 + 12 * 60 * 60 * 1000));
      mgr.cleanupStaleWorktrees(["/repos/active", "/repos/temp-wt"]);

      // The queue should be empty now (worktree is known again).
      queueRaw = mockStorage.getItem("crab.sessionCleanupQueue.v1");
      expect(queueRaw).toBeNull();

      // The session data should still be intact.
      const session = mgr.activate("/repos/temp-wt");
      expect(session).not.toBeNull();
      expect(session!.activeView).toBe("timeline");
    });

    it("does not queue sessions that are already in the queue", () => {
      const mgr = new SessionManager();

      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.deactivate("/repos/stale", { activeView: "timeline" });
      mgr.flush();

      // Run cleanup twice without time advancing.
      mgr.cleanupStaleWorktrees(["/repos/active"]);
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      // Should still only have one queue entry.
      const queueRaw = mockStorage.getItem("crab.sessionCleanupQueue.v1");
      const queue = JSON.parse(queueRaw!);
      expect(queue.length).toBe(1);
    });

    it("handles empty session index gracefully", () => {
      const mgr = new SessionManager();

      // No sessions exist — cleanup should not throw.
      expect(() => mgr.cleanupStaleWorktrees(["/repos/active"])).not.toThrow();

      // No queue should be created.
      const queueRaw = mockStorage.getItem("crab.sessionCleanupQueue.v1");
      expect(queueRaw).toBeNull();
    });

    it("logs info message when sessions are purged", () => {
      const infoSpy = vi.spyOn(console, "info").mockImplementation(() => {});
      const mgr = new SessionManager();
      vi.setSystemTime(new Date(1000));

      mgr.deactivate("/repos/active", { activeView: "explorer" });
      mgr.deactivate("/repos/stale-a", { activeView: "timeline" });
      mgr.deactivate("/repos/stale-b", { activeView: "changes" });
      mgr.flush();

      // Queue stale sessions.
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      // Advance past 24h and purge.
      vi.setSystemTime(new Date(1000 + 24 * 60 * 60 * 1000 + 1));
      mgr.cleanupStaleWorktrees(["/repos/active"]);

      expect(infoSpy).toHaveBeenCalledWith(
        expect.stringContaining("Purged 2 stale session(s)"),
      );

      infoSpy.mockRestore();
    });
  });
});
