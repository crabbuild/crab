import { describe, it, expect, vi, beforeEach } from "vitest";

import {
  registerDaemonTerminalIpc,
  type IpcMainLike,
  type BrowserWindowLike,
  type AppLike,
  type IpcBridgeOptions,
  type RestoredSession,
} from "../electron/daemon/daemon-bridge";

import type { DaemonClient } from "../electron/daemon/daemon-client";
import type { CreateOrAttachResult } from "../electron/daemon/protocol";
import type { SessionInfo, TerminalSnapshot } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

function createMockIpcMain(): IpcMainLike & {
  handlers: Map<string, (...args: unknown[]) => unknown>;
  invoke: (channel: string, ...args: unknown[]) => Promise<unknown>;
} {
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  return {
    handlers,
    handle(channel: string, listener: (...args: unknown[]) => unknown) {
      handlers.set(channel, listener);
    },
    removeHandler(channel: string) {
      handlers.delete(channel);
    },
    async invoke(channel: string, ...args: unknown[]) {
      const handler = handlers.get(channel);
      if (!handler) throw new Error(`No handler for ${channel}`);
      return handler(null, ...args);
    },
  };
}

function createMockWindow(): BrowserWindowLike & {
  sentMessages: Array<{ channel: string; args: unknown[] }>;
} {
  const sentMessages: Array<{ channel: string; args: unknown[] }> = [];
  return {
    sentMessages,
    isDestroyed: () => false,
    webContents: {
      send(channel: string, ...args: unknown[]) {
        sentMessages.push({ channel, args });
      },
    },
  };
}

function createMockApp(): AppLike & {
  listeners: Map<string, Array<(...args: unknown[]) => void>>;
  emit: (event: string, ...args: unknown[]) => void;
} {
  const listeners = new Map<string, Array<(...args: unknown[]) => void>>();
  return {
    listeners,
    on(event: string, listener: (...args: unknown[]) => void) {
      const list = listeners.get(event) || [];
      list.push(listener);
      listeners.set(event, list);
    },
    emit(event: string, ...args: unknown[]) {
      const list = listeners.get(event) || [];
      for (const fn of list) fn(...args);
    },
  };
}

function createMockSnapshot(overrides: Partial<TerminalSnapshot> = {}): TerminalSnapshot {
  return {
    snapshotAnsi: "$ hello\r\n",
    scrollbackAnsi: "",
    rehydrateSequences: "\x1b[?2004h",
    cwd: "/home/user",
    modes: {
      bracketedPaste: true,
      mouseTracking: false,
      mouseTrackingMode: "none",
      sgrMouseMode: false,
      applicationCursor: false,
      alternateScreen: false,
    },
    cols: 120,
    rows: 30,
    scrollbackLines: 0,
    ...overrides,
  };
}

interface MockDaemonClient {
  request: ReturnType<typeof vi.fn>;
  notify: ReturnType<typeof vi.fn>;
  getMode: ReturnType<typeof vi.fn>;
  isConnected: boolean;
  disconnect: ReturnType<typeof vi.fn>;
  mapSessionId: ReturnType<typeof vi.fn>;
  unmapSessionId: ReturnType<typeof vi.fn>;
  getDaemonSessionId: ReturnType<typeof vi.fn>;
}

function createMockClient(overrides: Partial<MockDaemonClient> = {}): MockDaemonClient {
  return {
    request: vi.fn().mockResolvedValue({ isNew: true, snapshot: null, pid: 123, shellState: "pending" }),
    notify: vi.fn().mockResolvedValue(undefined),
    getMode: vi.fn().mockReturnValue("daemon"),
    isConnected: true,
    disconnect: vi.fn(),
    mapSessionId: vi.fn(),
    unmapSessionId: vi.fn(),
    getDaemonSessionId: vi.fn(),
    ...overrides,
  };
}

function createBridgeOpts(overrides: Partial<{
  ipcMain: ReturnType<typeof createMockIpcMain>;
  client: MockDaemonClient;
  window: ReturnType<typeof createMockWindow>;
  app: ReturnType<typeof createMockApp>;
  workspacePath: string | null;
  writeSessionLog: ReturnType<typeof vi.fn>;
}> = {}) {
  const ipcMain = overrides.ipcMain ?? createMockIpcMain();
  const client = overrides.client ?? createMockClient();
  const window = overrides.window ?? createMockWindow();
  const app = overrides.app ?? createMockApp();
  const workspacePath: string | null = "workspacePath" in overrides
    ? (overrides.workspacePath ?? null)
    : "/workspace/project";
  const writeSessionLog = overrides.writeSessionLog ?? vi.fn();

  const opts: IpcBridgeOptions = {
    ipcMain,
    client: client as unknown as DaemonClient,
    getMainWindow: () => window,
    app,
    getWorkspacePath: () => workspacePath,
    writeSessionLog,
  };

  return { ipcMain, client, window, app, opts, writeSessionLog };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("IPC Bridge", () => {
  // -------------------------------------------------------------------------
  // crab:term-spawn → createOrAttach
  // -------------------------------------------------------------------------

  describe("crab:term-spawn", () => {
    it("forwards spawn to client.request('createOrAttach') and returns renderer ID", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      const result = await ipcMain.invoke("crab:term-spawn", "/home/user");
      expect(result).toEqual({ id: "term-1" });

      expect(client.request).toHaveBeenCalledWith("createOrAttach", expect.objectContaining({
        sessionId: "/workspace/project@@term-1",
        cols: 120,
        rows: 30,
        cwd: "/home/user",
        shellReadySupported: false,
      }));
    });

    it("passes extraEnv and startupCommand to createOrAttach", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home", "zsh", { FOO: "bar" }, "ls -la");

      expect(client.request).toHaveBeenCalledWith("createOrAttach", expect.objectContaining({
        env: { FOO: "bar" },
        command: "ls -la",
      }));
    });

    it("forwards agentMeta to createOrAttach and sets shellReadySupported true", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      const agentMeta = { taskId: "task-123", agentType: "claude", worktreePath: "/workspace/project" };
      await ipcMain.invoke("crab:term-spawn", "/home", undefined, undefined, undefined, agentMeta);

      expect(client.request).toHaveBeenCalledWith("createOrAttach", expect.objectContaining({
        agentMeta,
        shellReadySupported: true,
      }));
    });

    it("maps renderer ID to daemon session ID", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      expect(client.mapSessionId).toHaveBeenCalledWith("term-1", "/workspace/project@@term-1");
    });

    it("increments renderer IDs for each spawn", async () => {
      const { ipcMain, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      const r1 = await ipcMain.invoke("crab:term-spawn", "/home");
      const r2 = await ipcMain.invoke("crab:term-spawn", "/home");

      expect(r1).toEqual({ id: "term-1" });
      expect(r2).toEqual({ id: "term-2" });
    });

    it("sends snapshot data to renderer when reattaching to existing session", async () => {
      const snapshot = createMockSnapshot();
      const client = createMockClient({
        request: vi.fn().mockResolvedValue({
          isNew: false,
          snapshot,
          pid: 456,
          shellState: "ready",
        } as CreateOrAttachResult),
      });
      const { ipcMain, window, opts } = createBridgeOpts({ client });
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      const dataMsg = window.sentMessages.find((m) => m.channel === "crab:term-data");
      expect(dataMsg).toBeDefined();
      expect(dataMsg!.args[0]).toEqual({
        id: "term-1",
        data: snapshot.snapshotAnsi + snapshot.rehydrateSequences,
      });
    });

    it("cleans up mappings on spawn failure", async () => {
      const client = createMockClient({
        request: vi.fn().mockRejectedValue(new Error("Daemon unreachable")),
      });
      const { ipcMain, opts } = createBridgeOpts({ client });
      registerDaemonTerminalIpc(opts);

      await expect(ipcMain.invoke("crab:term-spawn", "/home")).rejects.toThrow("Daemon unreachable");
      expect(client.unmapSessionId).toHaveBeenCalledWith("term-1");
    });

    it("uses plain rendererId as sessionId when no workspace path", async () => {
      const { ipcMain, client, opts } = createBridgeOpts({ workspacePath: null });
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      expect(client.request).toHaveBeenCalledWith("createOrAttach", expect.objectContaining({
        sessionId: "term-1",
      }));
    });
  });

  // -------------------------------------------------------------------------
  // crab:term-write → notify
  // -------------------------------------------------------------------------

  describe("crab:term-write", () => {
    it("forwards write as fire-and-forget notify", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      // Spawn first to establish mapping
      await ipcMain.invoke("crab:term-spawn", "/home");

      await ipcMain.invoke("crab:term-write", "term-1", "hello");

      expect(client.notify).toHaveBeenCalledWith("write", {
        sessionId: "/workspace/project@@term-1",
        data: "hello",
      });
    });

    it("silently ignores writes to unknown session IDs", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-write", "unknown-id", "data");

      expect(client.notify).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // crab:term-resize → notify
  // -------------------------------------------------------------------------

  describe("crab:term-resize", () => {
    it("forwards resize as fire-and-forget notify", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      await ipcMain.invoke("crab:term-resize", "term-1", 80, 24);

      expect(client.notify).toHaveBeenCalledWith("resize", {
        sessionId: "/workspace/project@@term-1",
        cols: 80,
        rows: 24,
      });
    });

    it("silently ignores resize for unknown session IDs", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-resize", "unknown-id", 80, 24);

      expect(client.notify).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // crab:term-kill → request
  // -------------------------------------------------------------------------

  describe("crab:term-kill", () => {
    it("forwards kill as a request and cleans up mappings", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      await ipcMain.invoke("crab:term-kill", "term-1");

      expect(client.request).toHaveBeenCalledWith("kill", {
        sessionId: "/workspace/project@@term-1",
      });
      expect(client.unmapSessionId).toHaveBeenCalledWith("term-1");
    });

    it("cleans up mappings even if kill request fails", async () => {
      const client = createMockClient({
        request: vi.fn()
          .mockResolvedValueOnce({ isNew: true, snapshot: null, pid: 1, shellState: "pending" })
          .mockRejectedValueOnce(new Error("Session not found")),
      });
      const { ipcMain, opts } = createBridgeOpts({ client });
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      // Should not throw even though kill fails
      await ipcMain.invoke("crab:term-kill", "term-1");

      expect(client.unmapSessionId).toHaveBeenCalledWith("term-1");
    });

    it("silently ignores kill for unknown session IDs", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-kill", "unknown-id");

      // request should not have been called (only the initial mock setup)
      expect(client.request).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Stream events → renderer
  // -------------------------------------------------------------------------

  describe("stream event forwarding", () => {
    it("forwards session data to renderer as crab:term-data", async () => {
      const { ipcMain, window, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      bridge.handleSessionData("/workspace/project@@term-1", "output text");

      const dataMsg = window.sentMessages.find((m) =>
        m.channel === "crab:term-data" && (m.args[0] as { data: string }).data === "output text",
      );
      expect(dataMsg).toBeDefined();
      expect(dataMsg!.args[0]).toEqual({ id: "term-1", data: "output text" });
    });

    it("forwards session exit to renderer as crab:term-exit", async () => {
      const { ipcMain, window, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      bridge.handleSessionExit("/workspace/project@@term-1", 0);

      const exitMsg = window.sentMessages.find((m) => m.channel === "crab:term-exit");
      expect(exitMsg).toBeDefined();
      expect(exitMsg!.args[0]).toEqual({ id: "term-1", exitCode: 0 });
    });

    it("cleans up mappings on session exit", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      bridge.handleSessionExit("/workspace/project@@term-1", 0);

      // Subsequent write should be ignored (mapping gone)
      await ipcMain.invoke("crab:term-write", "term-1", "data");
      expect(client.notify).not.toHaveBeenCalled();
    });

    it("ignores data events for unmapped session IDs", () => {
      const { window, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      bridge.handleSessionData("unknown-session", "data");

      expect(window.sentMessages).toHaveLength(0);
    });

    it("calls writeSessionLog for forwarded data", async () => {
      const writeSessionLog = vi.fn();
      const { ipcMain, opts } = createBridgeOpts({ writeSessionLog });
      const bridge = registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      bridge.handleSessionData("/workspace/project@@term-1", "log this");

      expect(writeSessionLog).toHaveBeenCalledWith("term-1", "log this");
    });
  });

  // -------------------------------------------------------------------------
  // Warm reattach
  // -------------------------------------------------------------------------

  describe("warmReattach", () => {
    it("lists sessions, filters by workspace, and reattaches", async () => {
      const workspacePath = "repo-abc::/workspace/project";
      const sessions: SessionInfo[] = [
        {
          sessionId: `${workspacePath}@@term-old-1`,
          state: "running",
          shellState: "ready",
          isAlive: true,
          pid: 100,
          cwd: "/workspace/project",
          cols: 120,
          rows: 30,
          createdAt: Date.now(),
        },
        {
          sessionId: "repo-xyz::/other/workspace@@term-other",
          state: "running",
          shellState: "ready",
          isAlive: true,
          pid: 200,
          cwd: "/other/workspace",
          cols: 80,
          rows: 24,
          createdAt: Date.now(),
        },
      ];

      const snapshot = createMockSnapshot();
      const client = createMockClient({
        request: vi.fn()
          .mockImplementation((type: string) => {
            if (type === "listSessions") return Promise.resolve(sessions);
            if (type === "createOrAttach") {
              return Promise.resolve({
                isNew: false,
                snapshot,
                pid: 100,
                shellState: "ready",
              } as CreateOrAttachResult);
            }
            if (type === "kill") return Promise.resolve({});
            return Promise.resolve({});
          }),
      });

      const { window, opts } = createBridgeOpts({ client, workspacePath });
      const bridge = registerDaemonTerminalIpc(opts);

      const results = await bridge.warmReattach();

      // Should only reattach the workspace session, not the other one
      expect(results).toHaveLength(1);
      expect(results[0].id).toBe("term-1");
      expect(results[0].snapshot).toEqual(snapshot);
      expect(results[0].error).toBeUndefined();

      // Should have emitted crab:term-restored
      const restoredMsg = window.sentMessages.find((m) => m.channel === "crab:term-restored");
      expect(restoredMsg).toBeDefined();
      expect((restoredMsg!.args[0] as { sessions: RestoredSession[] }).sessions).toHaveLength(1);
    });

    it("handles partial reattach failure without blocking healthy sessions", async () => {
      const workspacePath = "repo-abc::/workspace/project";
      const sessions: SessionInfo[] = [
        {
          sessionId: `${workspacePath}@@term-1`,
          state: "running",
          shellState: "ready",
          isAlive: true,
          pid: 100,
          cwd: "/workspace/project",
          cols: 120,
          rows: 30,
          createdAt: Date.now(),
        },
        {
          sessionId: `${workspacePath}@@term-2`,
          state: "running",
          shellState: "ready",
          isAlive: true,
          pid: 200,
          cwd: "/workspace/project",
          cols: 120,
          rows: 30,
          createdAt: Date.now(),
        },
      ];

      const snapshot = createMockSnapshot();
      let createOrAttachCallCount = 0;
      const client = createMockClient({
        request: vi.fn().mockImplementation((type: string) => {
          if (type === "listSessions") return Promise.resolve(sessions);
          if (type === "createOrAttach") {
            createOrAttachCallCount++;
            if (createOrAttachCallCount === 1) {
              return Promise.resolve({ isNew: false, snapshot, pid: 100, shellState: "ready" });
            }
            // Second session fails
            return Promise.reject(new Error("Session corrupted"));
          }
          if (type === "kill") return Promise.resolve({});
          return Promise.resolve({});
        }),
      });

      const { opts } = createBridgeOpts({ client, workspacePath });
      const bridge = registerDaemonTerminalIpc(opts);

      const results = await bridge.warmReattach();

      expect(results).toHaveLength(2);
      // First session succeeded
      expect(results[0].snapshot).toEqual(snapshot);
      expect(results[0].error).toBeUndefined();
      // Second session failed with error
      expect(results[1].snapshot).toBeNull();
      expect(results[1].error).toBe("Session corrupted");
    });

    it("returns empty array when no workspace path", async () => {
      const { opts } = createBridgeOpts({ workspacePath: null });
      const bridge = registerDaemonTerminalIpc(opts);

      const results = await bridge.warmReattach();
      expect(results).toEqual([]);
    });

    it("returns empty array when listSessions fails", async () => {
      const client = createMockClient({
        request: vi.fn().mockRejectedValue(new Error("Connection lost")),
      });
      const { opts } = createBridgeOpts({ client });
      const bridge = registerDaemonTerminalIpc(opts);

      const results = await bridge.warmReattach();
      expect(results).toEqual([]);
    });

    it("returns empty array when no workspace sessions exist", async () => {
      const client = createMockClient({
        request: vi.fn().mockImplementation((type: string) => {
          if (type === "listSessions") return Promise.resolve([]);
          return Promise.resolve({});
        }),
      });
      const { opts } = createBridgeOpts({ client });
      const bridge = registerDaemonTerminalIpc(opts);

      const results = await bridge.warmReattach();
      expect(results).toEqual([]);
    });

    it("kills orphaned sessions from removed worktrees before reattaching", async () => {
      const workspacePath = "repo-abc::/workspace/project";
      const orphanSessionId = "repo-abc::/workspace/removed@@orphan-1";
      const validSessionId = `${workspacePath}@@term-valid`;

      const snapshot = createMockSnapshot();
      // listSessions is called twice: once by reconcileOnStartup, once by warmReattach
      let listSessionsCallCount = 0;
      const client = createMockClient({
        request: vi.fn().mockImplementation((type: string) => {
          if (type === "listSessions") {
            listSessionsCallCount++;
            // Both calls return the same sessions; reconcileOnStartup kills the orphan
            return Promise.resolve([
              {
                sessionId: orphanSessionId,
                state: "running",
                shellState: "ready",
                isAlive: true,
                pid: 999,
                cwd: "/workspace/removed",
                cols: 80,
                rows: 24,
                createdAt: Date.now(),
              },
              {
                sessionId: validSessionId,
                state: "running",
                shellState: "ready",
                isAlive: true,
                pid: 100,
                cwd: "/workspace/project",
                cols: 120,
                rows: 30,
                createdAt: Date.now(),
              },
            ] as SessionInfo[]);
          }
          if (type === "kill") return Promise.resolve({});
          if (type === "createOrAttach") {
            return Promise.resolve({ isNew: false, snapshot, pid: 100, shellState: "ready" } as CreateOrAttachResult);
          }
          return Promise.resolve({});
        }),
      });

      const { opts } = createBridgeOpts({ client, workspacePath });
      const bridge = registerDaemonTerminalIpc(opts);

      const results = await bridge.warmReattach();

      // reconcileOnStartup should have killed the orphan session
      expect(client.request).toHaveBeenCalledWith("kill", { sessionId: orphanSessionId });

      // Only the valid session should be reattached
      expect(results).toHaveLength(1);
      expect(results[0].error).toBeUndefined();
    });
  });

  // -------------------------------------------------------------------------
  // before-quit
  // -------------------------------------------------------------------------

  describe("before-quit", () => {
    it("sends checkpoint notifications and disconnects on before-quit", async () => {
      const { ipcMain, client, app, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      // Spawn a session to have something to checkpoint
      await ipcMain.invoke("crab:term-spawn", "/home");

      // Trigger before-quit
      app.emit("before-quit");

      // Give the async handler a tick to run
      await new Promise((r) => setTimeout(r, 10));

      expect(client.notify).toHaveBeenCalledWith("checkpoint", {
        sessionId: "/workspace/project@@term-1",
      });
      expect(client.disconnect).toHaveBeenCalled();
    });

    it("does not kill the daemon on before-quit", async () => {
      const { ipcMain, client, app, opts } = createBridgeOpts();
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      app.emit("before-quit");

      await new Promise((r) => setTimeout(r, 10));

      // Should NOT have sent a shutdown or kill request
      expect(client.request).not.toHaveBeenCalledWith("shutdown", expect.anything());
    });

    it("handles errors gracefully during before-quit", async () => {
      const client = createMockClient({
        request: vi.fn().mockResolvedValue({ isNew: true, snapshot: null, pid: 1, shellState: "pending" }),
        notify: vi.fn().mockRejectedValue(new Error("Socket closed")),
      });
      const { ipcMain, app, opts } = createBridgeOpts({ client });
      registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      // Should not throw
      app.emit("before-quit");
      await new Promise((r) => setTimeout(r, 10));

      // disconnect should still be called even if notify fails
      expect(client.disconnect).toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Renderer ID → daemon session ID mapping
  // -------------------------------------------------------------------------

  describe("session ID mapping", () => {
    it("maintains bidirectional mapping between renderer and daemon IDs", async () => {
      const { ipcMain, client, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");

      // Data sent to daemon session ID should be forwarded with renderer ID
      const window = createMockWindow();
      // Re-create with the window we can inspect
      const { ipcMain: ipc2, opts: opts2 } = createBridgeOpts({ client, window });
      const bridge2 = registerDaemonTerminalIpc(opts2);

      await ipc2.invoke("crab:term-spawn", "/home");
      bridge2.handleSessionData("/workspace/project@@term-1", "test");

      expect(window.sentMessages[0].args[0]).toEqual({ id: "term-1", data: "test" });
    });
  });

  // -------------------------------------------------------------------------
  // Dispose
  // -------------------------------------------------------------------------

  describe("dispose", () => {
    it("removes all IPC handlers", () => {
      const { ipcMain, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      bridge.dispose();

      expect(ipcMain.handlers.has("crab:term-spawn")).toBe(false);
      expect(ipcMain.handlers.has("crab:term-write")).toBe(false);
      expect(ipcMain.handlers.has("crab:term-resize")).toBe(false);
      expect(ipcMain.handlers.has("crab:term-kill")).toBe(false);
    });

    it("stops forwarding events after dispose", async () => {
      const { ipcMain, window, opts } = createBridgeOpts();
      const bridge = registerDaemonTerminalIpc(opts);

      await ipcMain.invoke("crab:term-spawn", "/home");
      bridge.dispose();

      bridge.handleSessionData("/workspace/project@@term-1", "should be ignored");

      // No messages should have been sent after dispose
      // (only the initial spawn response, no data forwarding)
      const dataMessages = window.sentMessages.filter((m) => m.channel === "crab:term-data");
      expect(dataMessages).toHaveLength(0);
    });
  });

});
