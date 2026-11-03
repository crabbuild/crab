/**
 * Unit tests for RemoteWorkspaceManager.executeCommand and killCommand —
 * streaming command execution with SIGTERM/SIGKILL termination.
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

describe("RemoteWorkspaceManager.executeCommand", () => {
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

    // Open a workspace so the manager has state
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

    const streamId = manager.executeCommand("profile-1", "cargo build", "/remote/workspace");

    expect(streamId).toBeDefined();
    expect(typeof streamId).toBe("string");
    expect(streamId.length).toBeGreaterThan(0);
  });

  it("calls exec_command RPC with correct params", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    const streamId = manager.executeCommand("profile-1", "npm run build", "/remote/workspace/app");

    expect(router._calls[0].method).toBe("exec_command");
    expect(router._calls[0].params).toMatchObject({
      command: "npm run build",
      cwd: "/remote/workspace/app",
      stream_id: streamId,
    });
  });

  it("emits stream-end with exit code on successful RPC completion", async () => {
    router._sendResult = { exit_code: 0 };

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.executeCommand("profile-1", "echo hello", "/tmp");

    await vi.advanceTimersByTimeAsync(0);

    expect(events).toHaveLength(1);
    expect(events[0]).toEqual({ streamId, exitCode: 0, error: undefined });
  });

  it("emits stream-end with non-zero exit code", async () => {
    router._sendResult = { exit_code: 1, error: "Build failed" };

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.executeCommand("profile-1", "cargo build", "/remote");

    await vi.advanceTimersByTimeAsync(0);

    expect(events).toHaveLength(1);
    expect(events[0]).toEqual({ streamId, exitCode: 1, error: "Build failed" });
  });

  it("emits stream-end with error on RPC failure", async () => {
    router._sendError = new Error("Agent unreachable");

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.executeCommand("profile-1", "ls", "/tmp");

    await vi.advanceTimersByTimeAsync(0);

    expect(events).toHaveLength(1);
    expect(events[0]).toEqual({ streamId, error: "Agent unreachable" });
  });

  it("forwards exec_output notifications as stream-data events", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    const streamId = manager.executeCommand("profile-1", "cargo build", "/remote");

    // Simulate exec_output notification from the agent
    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_output",
      params: { stream_id: streamId, fd: "stdout", data: "Compiling crab v0.1.0\n" },
    });

    expect(streamDataEvents).toHaveLength(1);
    expect(streamDataEvents[0]).toEqual({
      streamId,
      data: "Compiling crab v0.1.0\n",
    });
  });

  it("forwards stderr exec_output notifications as stream-data events", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    const streamId = manager.executeCommand("profile-1", "cargo build", "/remote");

    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_output",
      params: { stream_id: streamId, fd: "stderr", data: "warning: unused variable\n" },
    });

    expect(streamDataEvents).toHaveLength(1);
    expect(streamDataEvents[0]).toEqual({
      streamId,
      data: "warning: unused variable\n",
    });
  });

  it("handles exec_done notification and emits stream-end", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    const streamId = manager.executeCommand("profile-1", "make test", "/remote");

    // Simulate exec_done notification from the agent
    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_done",
      params: { stream_id: streamId, exit_code: 0 },
    });

    expect(streamEndEvents).toHaveLength(1);
    expect(streamEndEvents[0]).toEqual({
      streamId,
      exitCode: 0,
    });
    expect(manager.isExecActive(streamId)).toBe(false);
  });

  it("handles exec_done with non-zero exit code", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    const streamId = manager.executeCommand("profile-1", "false", "/remote");

    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_done",
      params: { stream_id: streamId, exit_code: 137 },
    });

    expect(streamEndEvents).toHaveLength(1);
    expect(streamEndEvents[0]).toEqual({
      streamId,
      exitCode: 137,
    });
  });

  it("ignores exec_output notifications for unknown stream IDs", () => {
    const streamDataEvents: unknown[] = [];
    manager.on("stream-data", (e) => streamDataEvents.push(e));

    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_output",
      params: { stream_id: "nonexistent", fd: "stdout", data: "should be ignored" },
    });

    expect(streamDataEvents).toHaveLength(0);
  });

  it("ignores exec_done notifications for unknown stream IDs", () => {
    const streamEndEvents: unknown[] = [];
    manager.on("stream-end", (e) => streamEndEvents.push(e));

    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_done",
      params: { stream_id: "nonexistent", exit_code: 0 },
    });

    expect(streamEndEvents).toHaveLength(0);
  });

  it("throws when not connected", () => {
    sessionManager._setStatus("profile-1", "disconnected");

    expect(() => {
      manager.executeCommand("profile-1", "ls", "/tmp");
    }).toThrow(/SSH connection not available/);
  });

  it("isExecActive returns true for active streams", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const streamId = manager.executeCommand("profile-1", "sleep 10", "/tmp");

    expect(manager.isExecActive(streamId)).toBe(true);
  });

  it("isExecActive returns false for unknown streams", () => {
    expect(manager.isExecActive("nonexistent")).toBe(false);
  });

  it("dispose cancels all active exec streams", () => {
    router.sendToRemoteAgent = () => new Promise(() => {});

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    const streamId = manager.executeCommand("profile-1", "sleep 100", "/tmp");

    expect(manager.isExecActive(streamId)).toBe(true);

    manager.dispose();

    expect(events.some((e: any) => e.streamId === streamId && e.error === "Operation cancelled: shutdown")).toBe(true);
  });
});

describe("RemoteWorkspaceManager.killCommand", () => {
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

    // Open a workspace
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

  it("sends SIGTERM to the remote process", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    const streamId = manager.executeCommand("profile-1", "sleep 100", "/tmp");
    router._calls = [];

    // Override to track kill calls
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return Promise.resolve({ success: true });
    };

    manager.killCommand(streamId);

    expect(router._calls[0].method).toBe("exec_kill");
    expect(router._calls[0].params).toMatchObject({
      stream_id: streamId,
      signal: "SIGTERM",
    });
  });

  it("sends SIGKILL after 5 seconds if process does not exit", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    const streamId = manager.executeCommand("profile-1", "sleep 100", "/tmp");
    router._calls = [];

    // Override to track kill calls
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return Promise.resolve({ success: true });
    };

    manager.killCommand(streamId);

    // Only SIGTERM sent so far
    expect(router._calls).toHaveLength(1);
    expect(router._calls[0].params).toMatchObject({ signal: "SIGTERM" });

    // Advance 5 seconds — SIGKILL should be sent
    vi.advanceTimersByTime(5_000);

    expect(router._calls).toHaveLength(2);
    expect(router._calls[1].method).toBe("exec_kill");
    expect(router._calls[1].params).toMatchObject({
      stream_id: streamId,
      signal: "SIGKILL",
    });
  });

  it("does not send SIGKILL if process exits within 5 seconds", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    const streamId = manager.executeCommand("profile-1", "sleep 100", "/tmp");
    router._calls = [];

    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return Promise.resolve({ success: true });
    };

    manager.killCommand(streamId);

    // Simulate the process exiting via exec_done notification before 5s
    router.emit("notification", {
      profileId: "profile-1",
      method: "exec_done",
      params: { stream_id: streamId, exit_code: 143 },
    });

    // Advance past the 5-second grace period
    vi.advanceTimersByTime(5_000);

    // Only SIGTERM should have been sent (no SIGKILL)
    const killCalls = router._calls.filter((c) => c.method === "exec_kill");
    expect(killCalls).toHaveLength(1);
    expect(killCalls[0].params).toMatchObject({ signal: "SIGTERM" });
  });

  it("is a no-op for unknown stream IDs", () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return Promise.resolve({ success: true });
    };

    manager.killCommand("nonexistent");

    expect(router._calls).toHaveLength(0);
  });

  it("handles SIGKILL RPC failure gracefully by ending the stream locally", async () => {
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      return new Promise(() => {});
    };

    const streamId = manager.executeCommand("profile-1", "sleep 100", "/tmp");
    router._calls = [];

    // SIGTERM succeeds, SIGKILL fails
    let callCount = 0;
    router.sendToRemoteAgent = (_pid: string, method: string, params: unknown) => {
      router._calls.push({ method, params });
      callCount++;
      if (callCount === 1) return Promise.resolve({ success: true }); // SIGTERM
      return Promise.reject(new Error("Connection lost")); // SIGKILL
    };

    const events: unknown[] = [];
    manager.on("stream-end", (e) => events.push(e));

    manager.killCommand(streamId);

    // Advance 5 seconds to trigger SIGKILL
    vi.advanceTimersByTime(5_000);

    // Allow the rejected promise to settle
    await vi.advanceTimersByTimeAsync(0);

    expect(events.some((e: any) => e.streamId === streamId && e.error === "Process killed (SIGKILL)")).toBe(true);
    expect(manager.isExecActive(streamId)).toBe(false);
  });
});
