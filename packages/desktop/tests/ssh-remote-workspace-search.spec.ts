/**
 * Unit tests for RemoteWorkspaceManager.searchWorkspace — streaming
 * search with 10,000 match cap and 60-second timeout.
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { EventEmitter } from "node:events";

// Mock electron before importing the module under test
vi.mock("electron", () => ({
  app: {
    getPath: () => "/tmp/test-user-data",
  },
}));

import { RemoteWorkspaceManager } from "../electron/ssh-remote-workspace";
import type { RpcRouter } from "../electron/ssh-router";
import type { SshSessionManager } from "../electron/ssh-manager";
import type { RemoteFileEditor } from "../electron/ssh-file-editor";

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
    sendToRemoteAgent(_profileId: string, method: string, params: unknown): Promise<unknown> {
      mock._calls.push({ method, params });
      if (mock._sendError) {
        return Promise.reject(mock._sendError);
      }
      return Promise.resolve(mock._sendResult);
    },
  });
  return mock as any;
}

function createMockFileEditor(): RemoteFileEditor {
  return {
    readFile: vi.fn(),
    writeFile: vi.fn(),
    createFile: vi.fn(),
    deleteFile: vi.fn(),
    renameFile: vi.fn(),
  } as any;
}

// ─── Tests ───────────────────────────────────────────────────────

describe("RemoteWorkspaceManager.searchWorkspace", () => {
  let manager: RemoteWorkspaceManager;
  let router: ReturnType<typeof createMockRouter>;
  let sessionManager: ReturnType<typeof createMockSessionManager>;
  let fileEditor: RemoteFileEditor;

  beforeEach(async () => {
    vi.useFakeTimers();
    router = createMockRouter();
    sessionManager = createMockSessionManager();
    fileEditor = createMockFileEditor();
    sessionManager._setStatus("profile-1", "connected");

    manager = new RemoteWorkspaceManager(
      router as any,
      sessionManager as any,
      fileEditor as any,
    );

    // Open a workspace so searchWorkspace has state to work with
    router._sendResult = {
      entries: [{ path: "/remote/workspace/file.ts", kind: "file", size: 100 }],
      total_entries: 1,
      watch_mode: "inotify",
    };
    await manager.openWorkspace("profile-1", "/remote/workspace");
    router._calls = [];
    router._sendResult = {};
  });

  afterEach(() => {
    manager.dispose();
    vi.useRealTimers();
  });

  it("returns a stream ID", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamId = manager.searchWorkspace("profile-1", "TODO");

    expect(streamId).toBeDefined();
    expect(typeof streamId).toBe("string");
    expect(streamId.length).toBeGreaterThan(0);
  });

  it("calls workspace_search RPC with correct params", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    manager.searchWorkspace("profile-1", "TODO", {
      path: "/remote/workspace/src",
      regex: true,
      caseSensitive: true,
      maxResults: 500,
    });

    expect(router._calls[0].method).toBe("workspace_search");
    expect(router._calls[0].params).toMatchObject({
      path: "/remote/workspace/src",
      query: "TODO",
      regex: true,
      case_sensitive: true,
      max_results: 500,
    });
  });

  it("uses workspace root path when no path option provided", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    manager.searchWorkspace("profile-1", "import");

    expect(router._calls[0].params).toMatchObject({
      path: "/remote/workspace",
      query: "import",
    });
  });

  it("caps maxResults at 10,000", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    manager.searchWorkspace("profile-1", "test", { maxResults: 50_000 });

    expect((router._calls[0].params as any).max_results).toBe(10_000);
  });

  it("emits stream-end on successful RPC completion", async () => {
    router._sendResult = { exit_code: 0, total_matches: 42 };

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.searchWorkspace("profile-1", "function");

    await vi.advanceTimersByTimeAsync(0);

    expect(events).toHaveLength(1);
    expect(events[0]).toEqual({ streamId, exitCode: 0, error: undefined });
  });

  it("emits stream-end with error on RPC failure", async () => {
    router._sendError = new Error("Agent unreachable");

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.searchWorkspace("profile-1", "class");

    await vi.advanceTimersByTimeAsync(0);

    expect(events).toHaveLength(1);
    expect(events[0]).toEqual({ streamId, error: "Agent unreachable" });
  });

  it("emits stream-end with timeout after 60 seconds", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.searchWorkspace("profile-1", "timeout-test");

    expect(manager.isSearchActive(streamId)).toBe(true);

    vi.advanceTimersByTime(60_000);

    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({
      streamId,
      error: "Search timed out after 60 seconds",
    });
    expect(manager.isSearchActive(streamId)).toBe(false);
  });

  it("forwards search_result notifications as stream-data events", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    const streamId = manager.searchWorkspace("profile-1", "pattern");

    // Simulate a search_result notification from the agent
    router.emit("notification", {
      profileId: "profile-1",
      method: "search_result",
      params: { stream_id: streamId, data: "file.ts:10: pattern match", match_count: 1 },
    });

    expect(streamDataEvents).toHaveLength(1);
    expect(streamDataEvents[0]).toEqual({
      streamId,
      data: "file.ts:10: pattern match",
    });
  });

  it("caps at 10,000 matches and emits stream-end", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamDataEvents: unknown[] = [];
    const streamEndEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    const streamId = manager.searchWorkspace("profile-1", "common");

    // Simulate notifications that push past the 10,000 cap
    for (let i = 0; i < 100; i++) {
      router.emit("notification", {
        profileId: "profile-1",
        method: "search_result",
        params: { stream_id: streamId, data: `batch ${i}`, match_count: 100 },
      });
    }

    // 100 batches × 100 matches = 10,000 — should cap
    expect(streamEndEvents).toHaveLength(1);
    expect(streamEndEvents[0]).toMatchObject({
      streamId,
      error: "Search capped at 10000 matches",
    });
    expect(manager.isSearchActive(streamId)).toBe(false);
  });

  it("ignores search_result notifications for unknown stream IDs", () => {
    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    router.emit("notification", {
      profileId: "profile-1",
      method: "search_result",
      params: { stream_id: "nonexistent", data: "should be ignored" },
    });

    expect(streamDataEvents).toHaveLength(0);
  });

  it("does not emit timeout if search completes before 60s", async () => {
    router._sendResult = { exit_code: 0 };

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    manager.searchWorkspace("profile-1", "quick");

    await vi.advanceTimersByTimeAsync(0);

    // Should have exactly one stream-end (success)
    expect(events).toHaveLength(1);
    expect((events[0] as any).error).toBeUndefined();

    // Advance past timeout — no additional events
    vi.advanceTimersByTime(60_000);
    expect(events).toHaveLength(1);
  });

  it("cancelSearch cancels an active search and emits stream-end", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.searchWorkspace("profile-1", "cancel-me");

    expect(manager.isSearchActive(streamId)).toBe(true);

    manager.cancelSearch(streamId);

    expect(manager.isSearchActive(streamId)).toBe(false);
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({
      streamId,
      error: "Search cancelled",
    });
  });

  it("cancelSearch is a no-op for unknown stream IDs", () => {
    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    manager.cancelSearch("nonexistent");

    expect(events).toHaveLength(0);
  });

  it("throws when no workspace is open for the profile", () => {
    sessionManager._setStatus("profile-2", "connected");

    expect(() => {
      manager.searchWorkspace("profile-2", "test");
    }).toThrow(/No workspace open for profile/);
  });

  it("throws when not connected", () => {
    sessionManager._setStatus("profile-1", "disconnected");

    expect(() => {
      manager.searchWorkspace("profile-1", "test");
    }).toThrow(/SSH connection not available/);
  });

  it("dispose cancels all active search streams", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.searchWorkspace("profile-1", "dispose-test");

    expect(manager.isSearchActive(streamId)).toBe(true);

    manager.dispose();

    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({
      streamId,
      error: "Operation cancelled: shutdown",
    });
  });
});
