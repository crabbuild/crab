/**
 * Verification tests for stale hunk detection via remote file watcher.
 *
 * Confirms that the ChangesView's stale hunk detection (via `isStaleHunkError`)
 * works for remote repos. The file watcher uses `getWatchPath(repo)` which
 * returns `repo.repo_id` for remote repos, and the remote provider emits
 * `fs/changed` events with `repoPath` set to the repo_id.
 *
 * The chain under test:
 *   1. Remote agent emits fs/changed notification
 *   2. RemoteProvider receives, debounces, re-emits with repoPath = repo_id
 *   3. main.ts bridges to renderer via webContents.send("crab:event", ...)
 *   4. useFileWatcher hook matches on repo_id and fires the callback
 *   5. If a staging op fails with stale_hunk error, isStaleHunkError catches it
 *
 * Validates: Requirements 6.5, 6.7
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import { withRepositoryRuntimes } from "./repository-runtime-fixture";
import { RemoteProvider } from "../electron/remote-provider";
import { getWatchPath } from "../src/hooks/useFileWatcher";
import { isStaleHunkError, STALE_HUNK_RPC_CODE } from "../src/ipc";

// ── Mock factories ───────────────────────────────────────────────

function makeMockRouter() {
  const emitter = new EventEmitter();
  return Object.assign(emitter, {
    route: vi.fn().mockResolvedValue(""),
    sendToRemoteAgent: vi.fn().mockResolvedValue({}),
  });
}

function makeMockFileEditor() {
  return {
    createFile: vi.fn().mockResolvedValue({}),
    renameFile: vi.fn().mockResolvedValue({}),
    deleteFile: vi.fn().mockResolvedValue({}),
    writeFile: vi.fn().mockResolvedValue({ success: true }),
    readFile: vi.fn().mockResolvedValue({ content: "" }),
  };
}

function makeMockGitManager() {
  return {};
}

function makeMockCapabilityNegotiator(supportsFileWatching = true) {
  return {
    isSupported: vi.fn().mockImplementation((_profileId: string, feature: string) => {
      if (feature === "watch_start") return supportsFileWatching;
      return false;
    }),
    getCached: vi.fn().mockReturnValue({
      protocol: 1,
      version: "2.0.0-alpha.1",
      methods: supportsFileWatching ? ["capabilities", "watch_start", "watch_stop"] : ["capabilities"],
    }),
  };
}

function makeMockSessionManager(status = "connected") {
  const emitter = new EventEmitter();
  return withRepositoryRuntimes(Object.assign(emitter, { repositoryWork: () => undefined,
    getStatus: vi.fn().mockReturnValue(status),
    getConnection: vi.fn().mockReturnValue(null),
    agentSession: vi.fn().mockReturnValue("ssh:1"),
  }));
}

// ── Tests ────────────────────────────────────────────────────────

describe("Remote stale hunk detection", () => {
  const REPO_ID = "ssh://profile-1/home/user/project";

  describe("getWatchPath returns repo_id for remote repos", () => {
    it("returns repo_id when repo is remote", () => {
      const repo = {
        repo_id: REPO_ID,
        is_remote: true,
        path: undefined,
        deferred: false,
      } as any;

      expect(getWatchPath(repo)).toBe(REPO_ID);
    });

    it("returns filesystem path when repo is local", () => {
      const repo = {
        repo_id: "local-repo-id",
        is_remote: false,
        path: "/home/user/project",
        deferred: false,
      } as any;

      expect(getWatchPath(repo)).toBe("/home/user/project");
    });

    it("returns undefined for deferred repos", () => {
      const repo = {
        repo_id: REPO_ID,
        is_remote: true,
        path: undefined,
        deferred: true,
      } as any;

      expect(getWatchPath(repo)).toBeUndefined();
    });
  });

  describe("isStaleHunkError detection", () => {
    it("detects stale hunk by numeric error code", () => {
      const err = { code: STALE_HUNK_RPC_CODE, message: "hunk mismatch" };
      expect(isStaleHunkError(err)).toBe(true);
    });

    it("detects stale hunk by data.kind field", () => {
      const err = { code: -1, data: { kind: "stale_hunk" }, message: "error" };
      expect(isStaleHunkError(err)).toBe(true);
    });

    it("detects stale hunk by message substring", () => {
      const err = { message: "Operation failed: stale_hunk detected" };
      expect(isStaleHunkError(err)).toBe(true);
    });

    it("returns false for non-stale-hunk errors", () => {
      const err = { code: 1003, message: "IO error" };
      expect(isStaleHunkError(err)).toBe(false);
    });

    it("returns false for null/undefined", () => {
      expect(isStaleHunkError(null)).toBe(false);
      expect(isStaleHunkError(undefined)).toBe(false);
    });

    it("returns false for non-object values", () => {
      expect(isStaleHunkError("error")).toBe(false);
      expect(isStaleHunkError(42)).toBe(false);
    });

    it("detects stale hunk error from remote agent response format", () => {
      // This is the exact format the remote agent returns (from rpc.rs)
      const err = {
        code: 1005,
        message: "Hunk context mismatch",
        data: {
          kind: "stale_hunk",
          path: "src/main.ts",
          expected_old_start: 10,
          expected_old_lines: 5,
        },
      };
      expect(isStaleHunkError(err)).toBe(true);
    });
  });

  describe("RemoteProvider emits fs/changed with repo_id as repoPath", () => {
    let provider: RemoteProvider;
    let mockRouter: ReturnType<typeof makeMockRouter>;
    let mockSessionManager: ReturnType<typeof makeMockSessionManager>;

    beforeEach(() => {
      vi.useFakeTimers();
      mockRouter = makeMockRouter();
      mockSessionManager = makeMockSessionManager();

      provider = new RemoteProvider({
        router: mockRouter as any,
        fileEditor: makeMockFileEditor() as any,
        gitManager: makeMockGitManager() as any,
        capabilityNegotiator: makeMockCapabilityNegotiator(true) as any,
        sessionManager: mockSessionManager as any,
      });
    });

    afterEach(() => {
      provider.dispose();
      vi.useRealTimers();
    });

    it("emits fs/changed with repoPath matching repo_id when remote file changes", async () => {
      // Start watching the remote repo
      mockRouter.route.mockResolvedValue({ watch_id: "w1" });
      await provider.watchStart(REPO_ID);

      const events: any[] = [];
      provider.on("fs/changed", (e) => events.push(e));

      // Simulate the remote agent sending an fs/changed notification
      // through the RpcRouter's notification stream
      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs/changed",
        params: { path: "/home/user/project/src/main.ts" },
      });

      // Advance past the debounce window (300ms)
      await vi.advanceTimersByTimeAsync(350);

      expect(events).toHaveLength(1);
      expect(events[0].type).toBe("fs/changed");
      // The repoPath must match the repo_id so that the renderer's
      // useFileWatcher hook (which compares params.repo_path === repoPath)
      // correctly triggers the ChangesView refresh.
      expect(events[0].repoPath).toBe(REPO_ID);
    });

    it("debounces rapid fs/changed notifications into a single event", async () => {
      mockRouter.route.mockResolvedValue({ watch_id: "w1" });
      await provider.watchStart(REPO_ID);

      const events: any[] = [];
      provider.on("fs/changed", (e) => events.push(e));

      // Simulate rapid file changes (e.g., git checkout touching many files)
      for (let i = 0; i < 5; i++) {
        mockRouter.emit("notification", {
          profileId: "profile-1",
          method: "fs/changed",
          params: { path: `/home/user/project/file${i}.ts` },
        });
        await vi.advanceTimersByTimeAsync(50);
      }

      // Advance past the debounce window
      await vi.advanceTimersByTimeAsync(350);

      // Should coalesce into a single event
      expect(events).toHaveLength(1);
      expect(events[0].repoPath).toBe(REPO_ID);
    });

    it("does not emit fs/changed for unwatched repos", async () => {
      const events: any[] = [];
      provider.on("fs/changed", (e) => events.push(e));

      // Emit notification without starting a watch
      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs/changed",
        params: { path: "/home/user/project/src/main.ts" },
      });

      await vi.advanceTimersByTimeAsync(350);

      expect(events).toHaveLength(0);
    });

    it("clears pending debounce timers on connection loss", async () => {
      mockRouter.route.mockResolvedValue({ watch_id: "w1" });
      await provider.watchStart(REPO_ID);

      const events: any[] = [];
      provider.on("fs/changed", (e) => events.push(e));

      // Emit a notification that starts a debounce timer
      mockRouter.emit("notification", {
        profileId: "profile-1",
        method: "fs/changed",
        params: { path: "/home/user/project/src/main.ts" },
      });

      // Before the debounce fires, simulate connection loss
      // (within the 300ms debounce window)
      await vi.advanceTimersByTimeAsync(100);
      mockSessionManager.emit("ssh:status-changed", {
        profileId: "profile-1",
        status: "disconnected",
      });

      // Advance past the original debounce window
      await vi.advanceTimersByTimeAsync(350);

      // The pending timer was cleared on disconnect, so no event fires
      expect(events).toHaveLength(0);
    });
  });

  describe("End-to-end: remote file change triggers stale hunk recovery", () => {
    it("stale hunk error from remote staging op is correctly identified", () => {
      // Simulate the error that would come back from a remote
      // git_stage_hunks call when the file was modified between
      // the diff render and the stage attempt.
      const remoteError = {
        code: 1005,
        data: {
          kind: "stale_hunk",
          path: "src/lib.rs",
          expected_old_start: 42,
          expected_old_lines: 7,
        },
        message: "Hunk context does not match current file content",
      };

      // isStaleHunkError should detect this regardless of transport
      expect(isStaleHunkError(remoteError)).toBe(true);
    });

    it("non-stale errors from remote ops are not misidentified", () => {
      // Permission denied on remote
      const permError = { code: 1003, message: "Permission denied: /etc/shadow" };
      expect(isStaleHunkError(permError)).toBe(false);

      // Repo not open
      const repoError = { code: 1004, message: "Repository not open" };
      expect(isStaleHunkError(repoError)).toBe(false);

      // Patch apply failure (different from stale hunk)
      const patchError = {
        code: 1006,
        data: { kind: "patch_apply", stderr: "patch does not apply" },
        message: "Patch apply failed",
      };
      expect(isStaleHunkError(patchError)).toBe(false);
    });

    it("getWatchPath produces the correct key for useFileWatcher matching", () => {
      // The useFileWatcher hook compares `params.repo_path === repoPath`
      // where repoPath comes from getWatchPath(repo). For remote repos,
      // this must be the repo_id so it matches the event emitted by
      // the RemoteProvider (which sets repoPath to the repo_id).
      const remoteRepo = {
        repo_id: "ssh://gpu-server/home/dev/ml-project",
        is_remote: true,
        path: undefined,
        deferred: false,
      } as any;

      const watchPath = getWatchPath(remoteRepo);

      // This is what the RemoteProvider emits as evt.repoPath
      // and what main.ts sends as params.repo_path
      expect(watchPath).toBe("ssh://gpu-server/home/dev/ml-project");

      // Verify the matching logic: the renderer receives
      // { method: "fs/changed", params: { repo_path: "ssh://gpu-server/..." } }
      // and useFileWatcher checks params.repo_path === repoPath
      const simulatedEvent = {
        method: "fs/changed",
        params: { repo_path: "ssh://gpu-server/home/dev/ml-project" },
      };
      expect(simulatedEvent.params.repo_path).toBe(watchPath);
    });
  });
});
