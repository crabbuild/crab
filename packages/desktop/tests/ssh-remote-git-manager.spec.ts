/**
 * Unit tests for RemoteGitManager — the high-level git operations
 * manager that wraps RPC calls with timeout, streaming, and validation.
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { EventEmitter } from "node:events";
import { RemoteGitManager } from "../electron/ssh-remote-git";
import type { RpcRouter } from "../electron/ssh-router";
import type { SshSessionManager } from "../electron/ssh-manager";

// ─── Mock Infrastructure ─────────────────────────────────────────

function createMockSessionManager(): SshSessionManager & { _setStatus: (id: string, s: string) => void } {
  const statuses = new Map<string, string>();
  return {
    _setStatus(id: string, s: string) {
      statuses.set(id, s);
    },
    getStatus(profileId: string) {
      return statuses.get(profileId) ?? "disconnected";
    },
  } as any;
}

function createMockRouter(): RpcRouter & EventEmitter & {
  _sendResult: unknown;
  _sendError: Error | null;
  _calls: Array<{ method: string; params: unknown }>;
} {
  const emitter = new EventEmitter();
  const mock = Object.assign(emitter, {
    _sendResult: {} as unknown,
    _sendError: null as Error | null,
    _calls: [] as Array<{ method: string; params: unknown }>,
    sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
      mock._calls.push({ method, params });
      if (mock._sendError) {
        return Promise.reject(mock._sendError);
      }
      return Promise.resolve(mock._sendResult);
    },
  });
  return mock as any;
}

// ─── Tests ───────────────────────────────────────────────────────

describe("RemoteGitManager", () => {
  let manager: RemoteGitManager;
  let router: ReturnType<typeof createMockRouter>;
  let sessionManager: ReturnType<typeof createMockSessionManager>;

  beforeEach(() => {
    vi.useFakeTimers();
    router = createMockRouter();
    sessionManager = createMockSessionManager();
    sessionManager._setStatus("profile-1", "connected");
    manager = new RemoteGitManager(router as any, sessionManager as any);
  });

  afterEach(() => {
    manager.cancelAllStreams();
    vi.useRealTimers();
  });

  describe("detectGitRepo", () => {
    it("returns git repo info when detected", async () => {
      router._sendResult = {
        is_git_repo: true,
        root: "/home/user/project",
        branch: "main",
        remote_url: "https://github.com/user/project.git",
      };

      const result = await manager.detectGitRepo("profile-1", "/home/user/project");

      expect(result).toEqual({
        isGitRepo: true,
        root: "/home/user/project",
        branch: "main",
        remoteUrl: "https://github.com/user/project.git",
      });
      expect(router._calls[0].method).toBe("git_detect");
      expect(router._calls[0].params).toEqual({ path: "/home/user/project" });
    });

    it("returns false when not a git repo", async () => {
      router._sendResult = { is_git_repo: false };

      const result = await manager.detectGitRepo("profile-1", "/tmp/random");

      expect(result).toEqual({
        isGitRepo: false,
        root: undefined,
        branch: undefined,
        remoteUrl: undefined,
      });
    });

    it("throws when not connected", async () => {
      sessionManager._setStatus("profile-1", "disconnected");

      await expect(
        manager.detectGitRepo("profile-1", "/home/user/project"),
      ).rejects.toThrow(/SSH connection status is "disconnected"/);
    });

    it("times out after 120 seconds", async () => {
      // Make the RPC call hang forever
      router.sendToRemoteAgent = () => new Promise(() => {});

      const promise = manager.detectGitRepo("profile-1", "/home/user/project");

      vi.advanceTimersByTime(120_000);

      await expect(promise).rejects.toThrow(/timed out after 120 seconds/);
    });
  });

  describe("getStatus", () => {
    it("returns full git status with file indicators", async () => {
      router._sendResult = {
        branch: "feature/auth",
        files: [
          { path: "src/auth.ts", status: "modified", staged: true },
          { path: "src/new.ts", status: "untracked", staged: false },
          { path: "src/deleted.ts", status: "deleted", staged: false },
        ],
        ahead: 3,
        behind: 1,
      };

      const result = await manager.getStatus("profile-1", "/home/user/project");

      expect(result.branch).toBe("feature/auth");
      expect(result.files).toHaveLength(3);
      expect(result.files[0]).toEqual({ path: "src/auth.ts", status: "modified", staged: true });
      expect(result.files[1]).toEqual({ path: "src/new.ts", status: "untracked", staged: false });
      expect(result.ahead).toBe(3);
      expect(result.behind).toBe(1);
      expect(router._calls[0].method).toBe("git_status_full");
      expect(router._calls[0].params).toEqual({ repo_path: "/home/user/project" });
    });

    it("handles empty status response", async () => {
      router._sendResult = { branch: "main", files: [], ahead: 0, behind: 0 };

      const result = await manager.getStatus("profile-1", "/home/user/project");

      expect(result.branch).toBe("main");
      expect(result.files).toEqual([]);
      expect(result.ahead).toBe(0);
      expect(result.behind).toBe(0);
    });

    it("defaults to 'unknown' branch when not provided", async () => {
      router._sendResult = {};

      const result = await manager.getStatus("profile-1", "/home/user/project");

      expect(result.branch).toBe("unknown");
      expect(result.files).toEqual([]);
    });
  });

  describe("executeOperation", () => {
    describe("commit", () => {
      it("throws when commit message is empty", async () => {
        await expect(
          manager.executeOperation("profile-1", {
            type: "commit",
            message: "",
            repoPath: "/home/user/project",
          }),
        ).rejects.toThrow(/Commit message is required/);
      });

      it("throws when commit message is whitespace only", async () => {
        await expect(
          manager.executeOperation("profile-1", {
            type: "commit",
            message: "   \n\t  ",
            repoPath: "/home/user/project",
          }),
        ).rejects.toThrow(/Commit message is required/);
      });

      it("throws when commit message is undefined", async () => {
        await expect(
          manager.executeOperation("profile-1", {
            type: "commit",
            repoPath: "/home/user/project",
          }),
        ).rejects.toThrow(/Commit message is required/);
      });

      it("succeeds with a valid commit message", async () => {
        router._sendResult = { exit_code: 0 };

        const streamId = await manager.executeOperation("profile-1", {
          type: "commit",
          message: "feat: add authentication",
          repoPath: "/home/user/project",
        });

        expect(streamId).toBeDefined();
        expect(typeof streamId).toBe("string");
        expect(router._calls[0].method).toBe("git_operation");
        expect(router._calls[0].params).toMatchObject({
          repo_path: "/home/user/project",
          operation: "commit",
          message: "feat: add authentication",
        });
      });
    });

    describe("push/pull streaming", () => {
      it("returns a stream ID for push operations", async () => {
        router._sendResult = { exit_code: 0 };

        const streamId = await manager.executeOperation("profile-1", {
          type: "push",
          repoPath: "/home/user/project",
        });

        expect(streamId).toBeDefined();
        expect(typeof streamId).toBe("string");
        expect(streamId.length).toBeGreaterThan(0);
      });

      it("emits stream-end on successful completion", async () => {
        router._sendResult = { exit_code: 0 };

        const events: unknown[] = [];
        manager.on("stream-end", (e) => events.push(e));

        const streamId = await manager.executeOperation("profile-1", {
          type: "push",
          repoPath: "/home/user/project",
        });

        // Allow the promise to resolve
        await vi.advanceTimersByTimeAsync(0);

        expect(events).toHaveLength(1);
        expect(events[0]).toEqual({ streamId, exitCode: 0, error: undefined });
      });

      it("emits stream-end with error on RPC failure", async () => {
        router._sendError = new Error("Network error");

        const events: unknown[] = [];
        manager.on("stream-end", (e) => events.push(e));

        const streamId = await manager.executeOperation("profile-1", {
          type: "pull",
          repoPath: "/home/user/project",
        });

        await vi.advanceTimersByTimeAsync(0);

        expect(events).toHaveLength(1);
        expect(events[0]).toEqual({ streamId, error: "Network error" });
      });

      it("forwards git_progress notifications as stream-data events", async () => {
        // Make the RPC call hang so we can test streaming
        router.sendToRemoteAgent = () => new Promise(() => {});

        const streamDataEvents: unknown[] = [];
        manager.on("stream-data", (e) => streamDataEvents.push(e));

        const streamId = await manager.executeOperation("profile-1", {
          type: "push",
          repoPath: "/home/user/project",
        });

        // Simulate a git_progress notification from the agent
        router.emit("notification", {
          profileId: "profile-1",
          method: "git_progress",
          params: { stream_id: streamId, data: "Counting objects: 50% (5/10)" },
        });

        expect(streamDataEvents).toHaveLength(1);
        expect(streamDataEvents[0]).toEqual({
          streamId,
          data: "Counting objects: 50% (5/10)",
        });
      });

      it("ignores notifications for unknown stream IDs", async () => {
        const streamDataEvents: unknown[] = [];
        manager.on("stream-data", (e) => streamDataEvents.push(e));

        // Emit a notification for a stream that doesn't exist
        router.emit("notification", {
          profileId: "profile-1",
          method: "git_progress",
          params: { stream_id: "nonexistent-stream", data: "some data" },
        });

        expect(streamDataEvents).toHaveLength(0);
      });

      it("ignores non-git_progress notifications", async () => {
        router.sendToRemoteAgent = () => new Promise(() => {});

        const streamDataEvents: unknown[] = [];
        manager.on("stream-data", (e) => streamDataEvents.push(e));

        await manager.executeOperation("profile-1", {
          type: "push",
          repoPath: "/home/user/project",
        });

        // Emit a different notification type
        router.emit("notification", {
          profileId: "profile-1",
          method: "progress",
          params: { op: "something_else" },
        });

        expect(streamDataEvents).toHaveLength(0);
      });
    });

    describe("timeout", () => {
      it("emits stream-end with timeout error after 120 seconds", async () => {
        // Make the RPC call hang forever
        router.sendToRemoteAgent = () => new Promise(() => {});

        const events: unknown[] = [];
        manager.on("stream-end", (e) => events.push(e));

        const streamId = await manager.executeOperation("profile-1", {
          type: "push",
          repoPath: "/home/user/project",
        });

        expect(manager.isStreamActive(streamId)).toBe(true);

        vi.advanceTimersByTime(120_000);

        expect(events).toHaveLength(1);
        expect(events[0]).toMatchObject({
          streamId,
          error: expect.stringContaining("timed out after 120 seconds"),
        });
        expect(manager.isStreamActive(streamId)).toBe(false);
      });

      it("does not emit timeout if operation completes before 120s", async () => {
        router._sendResult = { exit_code: 0 };

        const events: unknown[] = [];
        manager.on("stream-end", (e) => events.push(e));

        await manager.executeOperation("profile-1", {
          type: "pull",
          repoPath: "/home/user/project",
        });

        await vi.advanceTimersByTimeAsync(0);

        // Should have exactly one stream-end (success), not a timeout
        expect(events).toHaveLength(1);
        expect((events[0] as any).error).toBeUndefined();

        // Advance past timeout — no additional events
        vi.advanceTimersByTime(120_000);
        expect(events).toHaveLength(1);
      });
    });

    describe("checkout and stash", () => {
      it("sends checkout operation with branch", async () => {
        router._sendResult = { exit_code: 0 };

        await manager.executeOperation("profile-1", {
          type: "checkout",
          branch: "feature/new",
          repoPath: "/home/user/project",
        });

        expect(router._calls[0].params).toMatchObject({
          operation: "checkout",
          branch: "feature/new",
        });
      });

      it("sends stash operation with action", async () => {
        router._sendResult = { exit_code: 0 };

        await manager.executeOperation("profile-1", {
          type: "stash",
          stashAction: "save",
          repoPath: "/home/user/project",
        });

        expect(router._calls[0].params).toMatchObject({
          operation: "stash",
          stash_action: "save",
        });
      });
    });

    describe("connection validation", () => {
      it("throws when not connected", async () => {
        sessionManager._setStatus("profile-1", "reconnecting");

        await expect(
          manager.executeOperation("profile-1", {
            type: "push",
            repoPath: "/home/user/project",
          }),
        ).rejects.toThrow(/SSH connection status is "reconnecting"/);
      });
    });
  });

  describe("cancelStream", () => {
    it("cancels an active stream and emits stream-end", async () => {
      router.sendToRemoteAgent = () => new Promise(() => {});

      const events: unknown[] = [];
      manager.on("stream-end", (e) => events.push(e));

      const streamId = await manager.executeOperation("profile-1", {
        type: "push",
        repoPath: "/home/user/project",
      });

      expect(manager.isStreamActive(streamId)).toBe(true);

      manager.cancelStream(streamId);

      expect(manager.isStreamActive(streamId)).toBe(false);
      expect(events).toHaveLength(1);
      expect(events[0]).toMatchObject({
        streamId,
        error: "Operation cancelled",
      });
    });

    it("is a no-op for unknown stream IDs", () => {
      const events: unknown[] = [];
      manager.on("stream-end", (e) => events.push(e));

      manager.cancelStream("nonexistent");

      expect(events).toHaveLength(0);
    });
  });

  describe("cancelAllStreams", () => {
    it("cancels all active streams", async () => {
      router.sendToRemoteAgent = () => new Promise(() => {});

      const events: unknown[] = [];
      manager.on("stream-end", (e) => events.push(e));

      const id1 = await manager.executeOperation("profile-1", {
        type: "push",
        repoPath: "/home/user/project",
      });
      const id2 = await manager.executeOperation("profile-1", {
        type: "pull",
        repoPath: "/home/user/project",
      });

      manager.cancelAllStreams();

      expect(manager.isStreamActive(id1)).toBe(false);
      expect(manager.isStreamActive(id2)).toBe(false);
      expect(events).toHaveLength(2);
    });
  });
});
