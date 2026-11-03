import { describe, it, expect, vi, beforeEach } from "vitest";

import {
  registerAgentDashboardIpc,
  calculateElapsedWorkingMs,
  type AgentDashboardInfo,
} from "../electron/daemon/agent-dashboard-ipc";
import type { IpcMainLike, BrowserWindowLike } from "../electron/daemon/ipc-bridge";
import type { DaemonClient } from "../electron/daemon/daemon-client";
import type { SessionInfo, AgentActivityState } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Mocks
// ---------------------------------------------------------------------------

function createMockIpcMain(): IpcMainLike & { handlers: Map<string, (...args: unknown[]) => unknown> } {
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  return {
    handlers,
    handle(channel: string, listener: (...args: unknown[]) => unknown): void {
      handlers.set(channel, listener);
    },
    removeHandler(channel: string): void {
      handlers.delete(channel);
    },
  };
}

function createMockWindow(): BrowserWindowLike & { sentMessages: Array<{ channel: string; args: unknown[] }> } {
  const sentMessages: Array<{ channel: string; args: unknown[] }> = [];
  return {
    sentMessages,
    isDestroyed: () => false,
    webContents: {
      send(channel: string, ...args: unknown[]): void {
        sentMessages.push({ channel, args });
      },
    },
  };
}

function createMockClient(sessions: SessionInfo[] = []): DaemonClient {
  return {
    getAgentSessions: vi.fn().mockResolvedValue(sessions),
  } as unknown as DaemonClient;
}

function makeAgentSession(overrides: Partial<SessionInfo> & {
  sessionId: string;
  taskId: string;
  agentType: string;
  worktreePath: string;
  activityState: AgentActivityState;
  stateChangedAt: number;
}): SessionInfo {
  return {
    sessionId: overrides.sessionId,
    state: overrides.state ?? "running",
    shellState: overrides.shellState ?? "ready",
    isAlive: overrides.isAlive ?? true,
    pid: overrides.pid ?? 1234,
    cwd: overrides.cwd ?? "/tmp",
    cols: overrides.cols ?? 80,
    rows: overrides.rows ?? 24,
    createdAt: overrides.createdAt ?? Date.now() - 60000,
    agentMeta: {
      taskId: overrides.taskId,
      agentType: overrides.agentType,
      worktreePath: overrides.worktreePath,
      activityState: overrides.activityState,
      stateChangedAt: overrides.stateChangedAt,
    },
  };
}

// ---------------------------------------------------------------------------
// Tests: calculateElapsedWorkingMs
// ---------------------------------------------------------------------------

describe("calculateElapsedWorkingMs", () => {
  it("returns elapsed time when state is working", () => {
    const now = 10000;
    const stateChangedAt = 7000;
    const elapsed = calculateElapsedWorkingMs("working", stateChangedAt, now);
    expect(elapsed).toBe(3000);
  });

  it("returns 0 when state is not working", () => {
    const now = 10000;
    const stateChangedAt = 7000;
    expect(calculateElapsedWorkingMs("waiting", stateChangedAt, now)).toBe(0);
    expect(calculateElapsedWorkingMs("idle", stateChangedAt, now)).toBe(0);
    expect(calculateElapsedWorkingMs("exited", stateChangedAt, now)).toBe(0);
  });

  it("returns 0 when stateChangedAt is in the future (clock skew)", () => {
    const now = 5000;
    const stateChangedAt = 8000;
    const elapsed = calculateElapsedWorkingMs("working", stateChangedAt, now);
    expect(elapsed).toBe(0);
  });

  it("returns 0 when stateChangedAt equals now", () => {
    const elapsed = calculateElapsedWorkingMs("working", 5000, 5000);
    expect(elapsed).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// Tests: registerAgentDashboardIpc
// ---------------------------------------------------------------------------

describe("registerAgentDashboardIpc", () => {
  let ipcMain: ReturnType<typeof createMockIpcMain>;
  let window: ReturnType<typeof createMockWindow>;

  beforeEach(() => {
    ipcMain = createMockIpcMain();
    window = createMockWindow();
  });

  describe("crab:agent-sessions handler", () => {
    it("registers the crab:agent-sessions IPC handler", () => {
      const client = createMockClient();
      registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      expect(ipcMain.handlers.has("crab:agent-sessions")).toBe(true);
    });

    it("returns agent sessions with elapsed working time", async () => {
      const stateChangedAt = Date.now() - 5000;
      const sessions: SessionInfo[] = [
        makeAgentSession({
          sessionId: "ws1@@agent-1",
          taskId: "task-1",
          agentType: "claude",
          worktreePath: "/projects/app",
          activityState: "working",
          stateChangedAt,
        }),
      ];

      const client = createMockClient(sessions);
      registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      const handler = ipcMain.handlers.get("crab:agent-sessions")!;
      const result = (await handler()) as AgentDashboardInfo[];

      expect(result).toHaveLength(1);
      expect(result[0]!.sessionId).toBe("ws1@@agent-1");
      expect(result[0]!.taskId).toBe("task-1");
      expect(result[0]!.agentType).toBe("claude");
      expect(result[0]!.worktreePath).toBe("/projects/app");
      expect(result[0]!.state).toBe("working");
      expect(result[0]!.stateChangedAt).toBe(stateChangedAt);
      expect(result[0]!.elapsedWorkingMs).toBeGreaterThanOrEqual(5000);
      expect(result[0]!.elapsedWorkingMs).toBeLessThan(6000);
    });

    it("returns 0 elapsed time for non-working sessions", async () => {
      const sessions: SessionInfo[] = [
        makeAgentSession({
          sessionId: "ws1@@agent-2",
          taskId: "task-2",
          agentType: "claude",
          worktreePath: "/projects/app",
          activityState: "waiting",
          stateChangedAt: Date.now() - 10000,
        }),
      ];

      const client = createMockClient(sessions);
      registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      const handler = ipcMain.handlers.get("crab:agent-sessions")!;
      const result = (await handler()) as AgentDashboardInfo[];

      expect(result[0]!.elapsedWorkingMs).toBe(0);
    });

    it("returns multiple agent sessions", async () => {
      const sessions: SessionInfo[] = [
        makeAgentSession({
          sessionId: "ws1@@agent-1",
          taskId: "task-1",
          agentType: "claude",
          worktreePath: "/projects/app1",
          activityState: "working",
          stateChangedAt: Date.now() - 3000,
        }),
        makeAgentSession({
          sessionId: "ws2@@agent-2",
          taskId: "task-2",
          agentType: "claude",
          worktreePath: "/projects/app2",
          activityState: "idle",
          stateChangedAt: Date.now() - 60000,
        }),
      ];

      const client = createMockClient(sessions);
      registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      const handler = ipcMain.handlers.get("crab:agent-sessions")!;
      const result = (await handler()) as AgentDashboardInfo[];

      expect(result).toHaveLength(2);
      expect(result[0]!.taskId).toBe("task-1");
      expect(result[1]!.taskId).toBe("task-2");
    });

    it("returns empty array when no agent sessions exist", async () => {
      const client = createMockClient([]);
      registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      const handler = ipcMain.handlers.get("crab:agent-sessions")!;
      const result = (await handler()) as AgentDashboardInfo[];

      expect(result).toEqual([]);
    });
  });

  describe("crab:agent-state-changed event forwarding", () => {
    it("forwards state change to renderer window", () => {
      const client = createMockClient();
      const { handleAgentStateChange } = registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      handleAgentStateChange("ws1@@agent-1", "working", "idle");

      expect(window.sentMessages).toHaveLength(1);
      expect(window.sentMessages[0]!.channel).toBe("crab:agent-state-changed");

      const payload = window.sentMessages[0]!.args[0] as any;
      expect(payload.sessionId).toBe("ws1@@agent-1");
      expect(payload.state).toBe("working");
      expect(payload.previousState).toBe("idle");
      expect(payload.timestamp).toBeTypeOf("number");
    });

    it("does not send when window is null", () => {
      const client = createMockClient();
      const { handleAgentStateChange } = registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => null,
      });

      handleAgentStateChange("ws1@@agent-1", "working", "idle");
      // No error thrown, no messages sent
      expect(window.sentMessages).toHaveLength(0);
    });

    it("does not send when window is destroyed", () => {
      const destroyedWindow: BrowserWindowLike = {
        isDestroyed: () => true,
        webContents: {
          send: vi.fn(),
        },
      };

      const client = createMockClient();
      const { handleAgentStateChange } = registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => destroyedWindow,
      });

      handleAgentStateChange("ws1@@agent-1", "exited", "working");
      expect(destroyedWindow.webContents.send).not.toHaveBeenCalled();
    });

    it("includes timestamp in forwarded event", () => {
      const client = createMockClient();
      const before = Date.now();

      const { handleAgentStateChange } = registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      handleAgentStateChange("ws1@@agent-1", "waiting", "working");

      const after = Date.now();
      const payload = window.sentMessages[0]!.args[0] as any;
      expect(payload.timestamp).toBeGreaterThanOrEqual(before);
      expect(payload.timestamp).toBeLessThanOrEqual(after);
    });
  });

  describe("dispose", () => {
    it("removes the crab:agent-sessions handler", () => {
      const client = createMockClient();
      const { dispose } = registerAgentDashboardIpc({
        ipcMain,
        client,
        getMainWindow: () => window,
      });

      expect(ipcMain.handlers.has("crab:agent-sessions")).toBe(true);

      dispose();

      expect(ipcMain.handlers.has("crab:agent-sessions")).toBe(false);
    });
  });
});
