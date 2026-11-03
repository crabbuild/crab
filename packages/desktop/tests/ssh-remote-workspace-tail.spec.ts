/**
 * Unit tests for RemoteWorkspaceManager.tailFile / stopTail — streaming
 * log tailing via agent RPC with "tail_start" / "tail_stop" and
 * "tail_output" notifications.
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
  _calls: Array<{ profileId: string; method: string; params: unknown }>;
} {
  const emitter = new EventEmitter();
  const mock = Object.assign(emitter, {
    _sendResult: {} as unknown,
    _sendError: null as Error | null,
    _calls: [] as Array<{ profileId: string; method: string; params: unknown }>,
    sendToRemoteAgent(profileId: string, method: string, params: unknown): Promise<unknown> {
      mock._calls.push({ profileId, method, params });
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

describe("RemoteWorkspaceManager.tailFile / stopTail", () => {
  let manager: RemoteWorkspaceManager;
  let router: ReturnType<typeof createMockRouter>;
  let sessionManager: ReturnType<typeof createMockSessionManager>;
  let fileEditor: RemoteFileEditor;

  beforeEach(async () => {
    router = createMockRouter();
    sessionManager = createMockSessionManager();
    fileEditor = createMockFileEditor();
    sessionManager._setStatus("profile-1", "connected");

    manager = new RemoteWorkspaceManager(
      router as any,
      sessionManager as any,
      fileEditor as any,
    );

    router._calls = [];
    router._sendResult = {};
  });

  afterEach(() => {
    manager.dispose();
  });

  it("tailFile returns a stream ID", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    expect(streamId).toBeDefined();
    expect(typeof streamId).toBe("string");
    expect(streamId.length).toBeGreaterThan(0);
  });

  it("tailFile calls tail_start RPC with correct params", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    expect(router._calls).toHaveLength(1);
    expect(router._calls[0].method).toBe("tail_start");
    expect(router._calls[0].params).toMatchObject({
      path: "/var/log/app.log",
      stream_id: streamId,
    });
  });

  it("tailFile marks the stream as active", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    expect(manager.isTailActive(streamId)).toBe(true);
  });

  it("tailFile throws when not connected", async () => {
    sessionManager._setStatus("profile-1", "disconnected");

    await expect(
      manager.tailFile("profile-1", "/var/log/app.log"),
    ).rejects.toThrow(/SSH connection not available/);
  });

  it("tailFile throws when agent RPC fails", async () => {
    router._sendError = new Error("Agent unreachable");

    await expect(
      manager.tailFile("profile-1", "/var/log/app.log"),
    ).rejects.toThrow(/Failed to start tailing.*Agent unreachable/);
  });

  it("forwards tail_output notifications as stream-data events", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    // Simulate a tail_output notification from the agent
    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: streamId, data: "2024-01-01 INFO: Server started\n" },
    });

    expect(streamDataEvents).toHaveLength(1);
    expect(streamDataEvents[0]).toEqual({
      streamId,
      data: "2024-01-01 INFO: Server started\n",
    });
  });

  it("delivers multiple lines as separate stream-data events", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: streamId, data: "line 1\n" },
    });

    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: streamId, data: "line 2\n" },
    });

    expect(streamDataEvents).toHaveLength(2);
    expect((streamDataEvents[0] as any).data).toBe("line 1\n");
    expect((streamDataEvents[1] as any).data).toBe("line 2\n");
  });

  it("ignores tail_output notifications for unknown stream IDs", async () => {
    await manager.tailFile("profile-1", "/var/log/app.log");

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: "nonexistent", data: "should be ignored" },
    });

    expect(streamDataEvents).toHaveLength(0);
  });

  it("stopTail calls tail_stop RPC and emits stream-end", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");
    router._calls = [];

    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    await manager.stopTail(streamId);

    expect(router._calls).toHaveLength(1);
    expect(router._calls[0].method).toBe("tail_stop");
    expect(router._calls[0].params).toMatchObject({ stream_id: streamId });

    expect(streamEndEvents).toHaveLength(1);
    expect(streamEndEvents[0]).toEqual({ streamId });
  });

  it("stopTail marks the stream as inactive", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    expect(manager.isTailActive(streamId)).toBe(true);

    await manager.stopTail(streamId);

    expect(manager.isTailActive(streamId)).toBe(false);
  });

  it("stopTail is a no-op for unknown stream IDs", async () => {
    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    await manager.stopTail("nonexistent");

    expect(streamEndEvents).toHaveLength(0);
    expect(router._calls).toHaveLength(0);
  });

  it("stopTail handles agent RPC failure gracefully", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");
    router._calls = [];
    router._sendError = new Error("Connection lost");

    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    // Should not throw — best-effort stop
    await manager.stopTail(streamId);

    // Still emits stream-end even if RPC fails
    expect(streamEndEvents).toHaveLength(1);
    expect(streamEndEvents[0]).toEqual({ streamId });
    expect(manager.isTailActive(streamId)).toBe(false);
  });

  it("no longer receives notifications after stopTail", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    await manager.stopTail(streamId);

    // Simulate a late notification after stop
    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: streamId, data: "late line\n" },
    });

    expect(streamDataEvents).toHaveLength(0);
  });

  it("dispose cancels all active tail streams", async () => {
    const streamId = await manager.tailFile("profile-1", "/var/log/app.log");

    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    expect(manager.isTailActive(streamId)).toBe(true);

    manager.dispose();

    expect(streamEndEvents).toHaveLength(1);
    expect(streamEndEvents[0]).toMatchObject({
      streamId,
      error: "Operation cancelled: shutdown",
    });
  });

  it("supports multiple concurrent tail streams", async () => {
    const streamId1 = await manager.tailFile("profile-1", "/var/log/app.log");
    const streamId2 = await manager.tailFile("profile-1", "/var/log/error.log");

    expect(streamId1).not.toBe(streamId2);
    expect(manager.isTailActive(streamId1)).toBe(true);
    expect(manager.isTailActive(streamId2)).toBe(true);

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    // Notification for stream 1
    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: streamId1, data: "app log line\n" },
    });

    // Notification for stream 2
    router.emit("notification", {
      profileId: "profile-1",
      method: "tail_output",
      params: { stream_id: streamId2, data: "error log line\n" },
    });

    expect(streamDataEvents).toHaveLength(2);
    expect((streamDataEvents[0] as any).streamId).toBe(streamId1);
    expect((streamDataEvents[1] as any).streamId).toBe(streamId2);

    // Stop only stream 1
    await manager.stopTail(streamId1);

    expect(manager.isTailActive(streamId1)).toBe(false);
    expect(manager.isTailActive(streamId2)).toBe(true);
  });
});
