/**
 * Unit tests for the RemoteFileWatcher class.
 *
 * Covers: watch start/stop via agent RPC, fs_event routing to sync
 * engine vs file browser, polling fallback notification, auto-reconnect
 * within 10s, file browser update within 2s.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import { RemoteFileWatcher } from "../electron/ssh-remote-file-watcher";

// ── Test helpers ─────────────────────────────────────────────────

function makeMockRouter() {
  const router = new EventEmitter() as EventEmitter & {
    sendToRemoteAgent: ReturnType<typeof vi.fn>;
  };
  router.sendToRemoteAgent = vi.fn().mockResolvedValue({
    watch_id: "watch-123",
    mode: "inotify",
    file_count: 1000,
  });
  return router;
}

function makeMockSessionManager() {
  const manager = new EventEmitter() as EventEmitter & {
    getStatus: ReturnType<typeof vi.fn>;
  };
  manager.getStatus = vi.fn().mockReturnValue("connected");
  return manager;
}

function makeMockSyncEngine(activePairs: Array<{ profileId: string; remoteDir: string; status: string }> = []) {
  return {
    listSyncPairs: vi.fn().mockImplementation((profileId: string) =>
      activePairs
        .filter((p) => p.profileId === profileId)
        .map((p) => ({
          id: "sync-" + p.remoteDir,
          profileId: p.profileId,
          localDir: "/local" + p.remoteDir,
          remoteDir: p.remoteDir,
          ignorePatterns: [],
          status: p.status,
          pendingChanges: 0,
        })),
    ),
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("RemoteFileWatcher", () => {
  let watcher: RemoteFileWatcher;
  let mockRouter: ReturnType<typeof makeMockRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;
  let mockSyncEngine: ReturnType<typeof makeMockSyncEngine>;

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRouter();
    mockSessionManager = makeMockSessionManager();
    mockSyncEngine = makeMockSyncEngine();
    watcher = new RemoteFileWatcher(
      mockRouter as any,
      mockSessionManager as any,
      mockSyncEngine as any,
    );
  });

  afterEach(() => {
    watcher.dispose();
    vi.useRealTimers();
  });

  describe("startWatching", () => {
    it("calls agent watch_start RPC with correct params", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace", {
        recursive: true,
        ignorePatterns: ["node_modules", ".git"],
      });

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "profile-1",
        "watch_start",
        {
          path: "/remote/workspace",
          recursive: true,
          ignore_patterns: ["node_modules", ".git"],
        },
      );
    });

    it("returns watch mode from agent response", async () => {
      const result = await watcher.startWatching("profile-1", "/remote/workspace");

      expect(result.watchId).toBe("watch-123");
      expect(result.mode).toBe("inotify");
      expect(result.fileCount).toBe(1000);
    });

    it("defaults recursive to true and ignorePatterns to empty", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "profile-1",
        "watch_start",
        {
          path: "/remote/workspace",
          recursive: true,
          ignore_patterns: [],
        },
      );
    });

    it("tracks the watch as active", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      expect(watcher.isWatching("profile-1", "/remote/workspace")).toBe(true);
      expect(watcher.getActiveWatches("profile-1")).toHaveLength(1);
    });

    it("emits polling-fallback when file count exceeds 50,000", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        watch_id: "watch-big",
        mode: "polling",
        file_count: 75_000,
      });

      const events: any[] = [];
      watcher.on("polling-fallback", (e) => events.push(e));

      await watcher.startWatching("profile-1", "/remote/large-workspace");

      expect(events).toHaveLength(1);
      expect(events[0].profileId).toBe("profile-1");
      expect(events[0].fileCount).toBe(75_000);
    });

    it("emits polling-fallback when mode is polling regardless of count", async () => {
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        watch_id: "watch-poll",
        mode: "polling",
        file_count: 30_000,
      });

      const events: any[] = [];
      watcher.on("polling-fallback", (e) => events.push(e));

      await watcher.startWatching("profile-1", "/remote/workspace");

      expect(events).toHaveLength(1);
    });
  });

  describe("stopWatching", () => {
    it("calls agent watch_stop RPC", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");
      await watcher.stopWatching("profile-1", "watch-123");

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "profile-1",
        "watch_stop",
        { watch_id: "watch-123" },
      );
    });

    it("removes the watch from active watches", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");
      await watcher.stopWatching("profile-1", "watch-123");

      expect(watcher.isWatching("profile-1", "/remote/workspace")).toBe(false);
      expect(watcher.getActiveWatches("profile-1")).toHaveLength(0);
    });

    it("handles RPC failure gracefully", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");
      mockRouter.sendToRemoteAgent.mockRejectedValueOnce(new Error("connection lost"));

      await expect(watcher.stopWatching("profile-1", "watch-123")).resolves.toBeUndefined();
      expect(watcher.isWatching("profile-1", "/remote/workspace")).toBe(false);
    });
  });

  describe("fs_event routing", () => {
    it("emits remote-file-changed for file browser when sync is not active", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      // Simulate fs_event notification from agent
      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "watch-123",
          events: [
            { path: "/remote/workspace/src/main.ts", kind: "modify", timestamp: 1700000000 },
          ],
        },
      });

      // Advance past the debounce delay (200ms)
      await vi.advanceTimersByTimeAsync(250);

      expect(events).toHaveLength(1);
      expect(events[0].profileId).toBe("profile-1");
      expect(events[0].path).toBe("/remote/workspace/src/main.ts");
      expect(events[0].event).toBe("modify");
    });

    it("does NOT emit remote-file-changed when sync is active for the path", async () => {
      // Set up sync engine with an active pair for this path
      const syncEngine = makeMockSyncEngine([
        { profileId: "profile-1", remoteDir: "/remote/workspace", status: "active" },
      ]);
      watcher.dispose();
      watcher = new RemoteFileWatcher(
        mockRouter as any,
        mockSessionManager as any,
        syncEngine as any,
      );

      await watcher.startWatching("profile-1", "/remote/workspace");

      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "watch-123",
          events: [
            { path: "/remote/workspace/src/main.ts", kind: "modify", timestamp: 1700000000 },
          ],
        },
      });

      await vi.advanceTimersByTimeAsync(250);

      expect(events).toHaveLength(0);
    });

    it("emits for file browser when sync pair exists but is paused", async () => {
      const syncEngine = makeMockSyncEngine([
        { profileId: "profile-1", remoteDir: "/remote/workspace", status: "paused" },
      ]);
      watcher.dispose();
      watcher = new RemoteFileWatcher(
        mockRouter as any,
        mockSessionManager as any,
        syncEngine as any,
      );

      await watcher.startWatching("profile-1", "/remote/workspace");

      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "watch-123",
          events: [
            { path: "/remote/workspace/src/main.ts", kind: "create", timestamp: 1700000000 },
          ],
        },
      });

      await vi.advanceTimersByTimeAsync(250);

      expect(events).toHaveLength(1);
    });

    it("ignores events for unknown watch IDs", async () => {
      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "unknown-watch",
          events: [
            { path: "/remote/workspace/file.txt", kind: "modify", timestamp: 1700000000 },
          ],
        },
      });

      await vi.advanceTimersByTimeAsync(250);

      expect(events).toHaveLength(0);
    });

    it("handles multiple events in a single notification", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "watch-123",
          events: [
            { path: "/remote/workspace/a.ts", kind: "create", timestamp: 1700000000 },
            { path: "/remote/workspace/b.ts", kind: "modify", timestamp: 1700000001 },
            { path: "/remote/workspace/c.ts", kind: "delete", timestamp: 1700000002 },
          ],
        },
      });

      await vi.advanceTimersByTimeAsync(250);

      expect(events).toHaveLength(3);
    });

    it("updates file browser within 2s of change detection", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      const startTime = Date.now();

      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "watch-123",
          events: [
            { path: "/remote/workspace/file.ts", kind: "modify", timestamp: 1700000000 },
          ],
        },
      });

      // Should emit within 2000ms (actually within 200ms debounce)
      await vi.advanceTimersByTimeAsync(200);

      expect(events).toHaveLength(1);
      const elapsed = Date.now() - startTime;
      expect(elapsed).toBeLessThanOrEqual(2000);
    });
  });

  describe("auto-reconnect watching", () => {
    it("preserves watch state on disconnection", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      // Simulate disconnection
      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      // Watch should be removed from active watches (connection is gone)
      expect(watcher.getActiveWatches("profile-1")).toHaveLength(0);
    });

    it("re-establishes watches on reconnection", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace", {
        recursive: true,
        ignorePatterns: [".git"],
      });

      // Simulate disconnection
      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      // Reset mock to track new calls
      mockRouter.sendToRemoteAgent.mockClear();
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        watch_id: "watch-456",
        mode: "inotify",
        file_count: 1000,
      });

      // Simulate reconnection
      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "connected",
      });

      // Allow async restoration to complete
      await vi.advanceTimersByTimeAsync(100);

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "profile-1",
        "watch_start",
        {
          path: "/remote/workspace",
          recursive: true,
          ignore_patterns: [".git"],
        },
      );
    });

    it("emits watch-reconnected on successful restoration", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      mockRouter.sendToRemoteAgent.mockResolvedValue({
        watch_id: "watch-new",
        mode: "inotify",
        file_count: 500,
      });

      const events: any[] = [];
      watcher.on("watch-reconnected", (e) => events.push(e));

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "connected",
      });

      await vi.advanceTimersByTimeAsync(100);

      expect(events).toHaveLength(1);
      expect(events[0].profileId).toBe("profile-1");
      expect(events[0].watchId).toBe("watch-new");
    });

    it("emits watch-mode-changed if mode differs after reconnection", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      // Reconnect with polling mode (was inotify before)
      mockRouter.sendToRemoteAgent.mockResolvedValue({
        watch_id: "watch-poll",
        mode: "polling",
        file_count: 60_000,
      });

      const events: any[] = [];
      watcher.on("watch-mode-changed", (e) => events.push(e));

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "connected",
      });

      await vi.advanceTimersByTimeAsync(100);

      expect(events).toHaveLength(1);
      expect(events[0].mode).toBe("polling");
      expect(events[0].previousMode).toBe("inotify");
    });

    it("emits watch-error if restoration fails", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      mockRouter.sendToRemoteAgent.mockRejectedValue(new Error("agent unavailable"));

      const errors: any[] = [];
      watcher.on("watch-error", (e) => errors.push(e));

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "connected",
      });

      await vi.advanceTimersByTimeAsync(100);

      expect(errors).toHaveLength(1);
      expect(errors[0].profileId).toBe("profile-1");
      expect(errors[0].error).toContain("Failed to restore watch");
    });

    it("completes reconnection within 10s timeout", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      // Simulate slow reconnection that takes 5s (within 10s limit)
      mockRouter.sendToRemoteAgent.mockImplementation(
        () => new Promise((resolve) =>
          setTimeout(() => resolve({
            watch_id: "watch-slow",
            mode: "inotify",
            file_count: 100,
          }), 5000),
        ),
      );

      const reconnected: any[] = [];
      watcher.on("watch-reconnected", (e) => reconnected.push(e));

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "connected",
      });

      await vi.advanceTimersByTimeAsync(6000);

      expect(reconnected).toHaveLength(1);
    });

    it("emits error if reconnection exceeds 10s timeout", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      // Simulate very slow reconnection that exceeds 10s
      mockRouter.sendToRemoteAgent.mockImplementation(
        () => new Promise((resolve) =>
          setTimeout(() => resolve({
            watch_id: "watch-timeout",
            mode: "inotify",
            file_count: 100,
          }), 15_000),
        ),
      );

      const errors: any[] = [];
      watcher.on("watch-error", (e) => errors.push(e));

      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "connected",
      });

      await vi.advanceTimersByTimeAsync(10_100);

      expect(errors).toHaveLength(1);
      expect(errors[0].error).toContain("10s timeout");
    });
  });

  describe("stopAllForProfile", () => {
    it("stops all watches for a specific profile", async () => {
      mockRouter.sendToRemoteAgent
        .mockResolvedValueOnce({ watch_id: "w1", mode: "inotify", file_count: 100 })
        .mockResolvedValueOnce({ watch_id: "w2", mode: "inotify", file_count: 200 })
        .mockResolvedValue({ success: true });

      await watcher.startWatching("profile-1", "/remote/path1");
      await watcher.startWatching("profile-1", "/remote/path2");

      expect(watcher.getActiveWatches("profile-1")).toHaveLength(2);

      await watcher.stopAllForProfile("profile-1");

      expect(watcher.getActiveWatches("profile-1")).toHaveLength(0);
    });
  });

  describe("dispose", () => {
    it("cleans up all resources", async () => {
      await watcher.startWatching("profile-1", "/remote/workspace");

      watcher.dispose();

      expect(watcher.getActiveWatches("profile-1")).toHaveLength(0);
    });

    it("removes notification listener from router", async () => {
      const listenerCount = mockRouter.listenerCount("notification");
      watcher.dispose();
      expect(mockRouter.listenerCount("notification")).toBe(listenerCount - 1);
    });

    it("removes status listener from session manager", async () => {
      const listenerCount = mockSessionManager.listenerCount("ssh:status-changed");
      watcher.dispose();
      expect(mockSessionManager.listenerCount("ssh:status-changed")).toBe(listenerCount - 1);
    });
  });

  describe("without sync engine", () => {
    it("always routes events to file browser when no sync engine provided", async () => {
      watcher.dispose();
      watcher = new RemoteFileWatcher(
        mockRouter as any,
        mockSessionManager as any,
        undefined,
      );

      await watcher.startWatching("profile-1", "/remote/workspace");

      const events: any[] = [];
      watcher.on("remote-file-changed", (e) => events.push(e));

      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs_event",
        params: {
          watch_id: "watch-123",
          events: [
            { path: "/remote/workspace/file.ts", kind: "modify", timestamp: 1700000000 },
          ],
        },
      });

      await vi.advanceTimersByTimeAsync(250);

      expect(events).toHaveLength(1);
    });
  });
});
