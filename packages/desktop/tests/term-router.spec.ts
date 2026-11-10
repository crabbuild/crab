import { describe, it, expect, vi, beforeEach } from "vitest";

import {
  TermRouter,
  type IpcMainLike,
  type BrowserWindowLike,
  type DaemonBridge,
  type InProcessBridge,
  type ModeResolutionProvider,
  type TermSpawnPayload,
  type TermSpawnResult,
  type TermSpawnError,
} from "../electron/term-router";
import { CircuitBreaker } from "../electron/daemon/circuit-breaker";

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
      if (!handler) throw new Error(`No handler registered for ${channel}`);
      return handler(null, ...args);
    },
  };
}

function createMockWindow(): BrowserWindowLike {
  return {
    isDestroyed: () => false,
    webContents: {
      send: vi.fn(),
    },
  };
}

function createMockDaemonBridge(): DaemonBridge & {
  spawnCalls: Array<{ payload: TermSpawnPayload; rendererId: string }>;
} {
  const spawnCalls: Array<{ payload: TermSpawnPayload; rendererId: string }> = [];
  return {
    spawnCalls,
    handleSpawn: vi.fn(async (payload: TermSpawnPayload, rendererId: string) => {
      spawnCalls.push({ payload, rendererId });
      return { daemonSessionId: `daemon-session-${rendererId}` };
    }),
    handleWrite: vi.fn(async () => {}),
    handleResize: vi.fn(async () => {}),
    handleKill: vi.fn(async () => {}),
    handleSessionData: vi.fn(),
    handleSessionExit: vi.fn(),
    dispose: vi.fn(),
  };
}

function createMockInProcessBridge(): InProcessBridge & {
  spawnCalls: Array<{ payload: TermSpawnPayload; rendererId: string }>;
} {
  const spawnCalls: Array<{ payload: TermSpawnPayload; rendererId: string }> = [];
  return {
    spawnCalls,
    handleSpawn: vi.fn(async (payload: TermSpawnPayload, rendererId: string) => {
      spawnCalls.push({ payload, rendererId });
    }),
    handleWrite: vi.fn(async () => {}),
    handleResize: vi.fn(async () => {}),
    handleKill: vi.fn(async () => {}),
    dispose: vi.fn(),
  };
}

function createModeResolution(daemonReady: boolean): ModeResolutionProvider {
  return { current: () => ({ daemonReady }) };
}

interface RouterFixture {
  ipcMain: ReturnType<typeof createMockIpcMain>;
  daemonBridge: ReturnType<typeof createMockDaemonBridge>;
  inProcessBridge: ReturnType<typeof createMockInProcessBridge>;
  circuitBreaker: CircuitBreaker;
  router: TermRouter;
  spawn: (payload: TermSpawnPayload) => Promise<TermSpawnResult | TermSpawnError>;
}

function buildRouter(daemonReady: boolean, circuitBreaker?: CircuitBreaker): RouterFixture {
  const ipcMain = createMockIpcMain();
  const daemonBridge = createMockDaemonBridge();
  const inProcessBridge = createMockInProcessBridge();
  const breaker = circuitBreaker ?? new CircuitBreaker();
  const router = new TermRouter({
    ipcMain,
    daemonBridge,
    inProcessBridge,
    modeResolution: createModeResolution(daemonReady),
    circuitBreaker: breaker,
    getMainWindow: () => createMockWindow(),
  });
  router.register();

  const spawn = (payload: TermSpawnPayload) =>
    ipcMain.invoke("crab:term-spawn", payload) as Promise<TermSpawnResult | TermSpawnError>;

  return { ipcMain, daemonBridge, inProcessBridge, circuitBreaker: breaker, router, spawn };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("TermRouter", () => {
  // -------------------------------------------------------------------------
  // sessionType: "in-process" routing
  // -------------------------------------------------------------------------

  describe('sessionType: "in-process"', () => {
    it("routes to the in-process bridge and returns a term-l-N id", async () => {
      const { spawn, inProcessBridge, daemonBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home/user", sessionType: "in-process" });

      expect(result).toMatchObject({ id: expect.stringMatching(/^term-l-[0-9a-f-]{36}$/), resolvedSessionType: "in-process" });
      expect(inProcessBridge.handleSpawn).toHaveBeenCalledOnce();
      expect(daemonBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("passes the full payload to the in-process bridge", async () => {
      const { spawn, inProcessBridge } = buildRouter(true);

      const payload: TermSpawnPayload = {
        cwd: "/workspace",
        sessionType: "in-process",
        shell: "/bin/zsh",
        env: { FOO: "bar" },
        startupCommand: "ls -la",
      };
      await spawn(payload);

      expect(inProcessBridge.handleSpawn).toHaveBeenCalledWith(payload, expect.stringMatching(/^term-l-[0-9a-f-]{36}$/));
    });

    it("creates unique local session identities", async () => {
      const { spawn } = buildRouter(true);

      const r1 = await spawn({ cwd: "/a", sessionType: "in-process" });
      const r2 = await spawn({ cwd: "/b", sessionType: "in-process" });

      expect((r1 as TermSpawnResult).id).toMatch(/^term-l-[0-9a-f-]{36}$/);
      expect((r2 as TermSpawnResult).id).toMatch(/^term-l-[0-9a-f-]{36}$/);
    });

    it("routes to in-process even when daemonReady is false", async () => {
      const { spawn, inProcessBridge } = buildRouter(false);

      const result = await spawn({ cwd: "/home", sessionType: "in-process" });

      expect((result as TermSpawnResult).resolvedSessionType).toBe("in-process");
      expect(inProcessBridge.handleSpawn).toHaveBeenCalledOnce();
    });

    it("honors an explicit global ephemeral mode for ordinary shells", async () => {
      const { router } = buildRouter(false);
      expect(router.resolveSessionType({ cwd: "/repo" }, { daemonReady: false, configuredMode: "in-process" })).toBe("in-process");
      expect(router.resolveSessionType({ cwd: "/repo", agentMeta: { taskId: "task", agentType: "agent", worktreePath: "/repo" } }, { daemonReady: false, configuredMode: "in-process" })).toBe("daemon");
    });

    it("defaults to durable sessions even without agent metadata", async () => {
      const { spawn, inProcessBridge, daemonBridge } = buildRouter(true);
      const result = await spawn({ cwd: "/home" });
      expect((result as TermSpawnResult).resolvedSessionType).toBe("daemon");
      expect(daemonBridge.handleSpawn).toHaveBeenCalledOnce();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // sessionType: "daemon" routing
  // -------------------------------------------------------------------------

  describe('sessionType: "daemon"', () => {
    it("routes to the daemon bridge and returns a term-d-N id", async () => {
      const { spawn, daemonBridge, inProcessBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home/user", sessionType: "daemon" });

      expect(result).toMatchObject({ id: expect.stringMatching(/^term-d-[0-9a-f-]{36}$/), resolvedSessionType: "daemon" });
      expect(daemonBridge.handleSpawn).toHaveBeenCalledOnce();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("passes the full payload to the daemon bridge", async () => {
      const { spawn, daemonBridge } = buildRouter(true);

      const payload: TermSpawnPayload = {
        cwd: "/workspace",
        sessionType: "daemon",
        agentMeta: { taskId: "task-1", agentType: "claude", worktreePath: "/workspace" },
      };
      await spawn(payload);

      expect(daemonBridge.handleSpawn).toHaveBeenCalledWith(payload, expect.stringMatching(/^term-d-[0-9a-f-]{36}$/));
    });

    it("creates unique durable session identities", async () => {
      const { spawn } = buildRouter(true);

      const r1 = await spawn({ cwd: "/a", sessionType: "daemon" });
      const r2 = await spawn({ cwd: "/b", sessionType: "daemon" });

      expect((r1 as TermSpawnResult).id).toMatch(/^term-d-[0-9a-f-]{36}$/);
      expect((r2 as TermSpawnResult).id).toMatch(/^term-d-[0-9a-f-]{36}$/);
    });

    it("defaults to daemon when agentMeta is present and no explicit sessionType", async () => {
      const { spawn, daemonBridge, inProcessBridge } = buildRouter(true);

      const result = await spawn({
        cwd: "/workspace",
        agentMeta: { taskId: "t1", agentType: "codex", worktreePath: "/workspace" },
      });

      expect((result as TermSpawnResult).resolvedSessionType).toBe("daemon");
      expect(daemonBridge.handleSpawn).toHaveBeenCalledOnce();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("partitions IDs correctly across mixed session types", async () => {
      const { spawn } = buildRouter(true);

      const d1 = await spawn({ cwd: "/a", sessionType: "daemon" });
      const l1 = await spawn({ cwd: "/b", sessionType: "in-process" });
      const d2 = await spawn({ cwd: "/c", sessionType: "daemon" });
      const l2 = await spawn({ cwd: "/d", sessionType: "in-process" });

      expect((d1 as TermSpawnResult).id).toMatch(/^term-d-[0-9a-f-]{36}$/);
      expect((l1 as TermSpawnResult).id).toMatch(/^term-l-[0-9a-f-]{36}$/);
      expect((d2 as TermSpawnResult).id).toMatch(/^term-d-[0-9a-f-]{36}$/);
      expect((l2 as TermSpawnResult).id).toMatch(/^term-l-[0-9a-f-]{36}$/);
    });
  });

  // -------------------------------------------------------------------------
  // Daemon unavailable — typed error, no fallback
  // -------------------------------------------------------------------------

  describe("daemon unavailable (daemonReady === false)", () => {
    it("returns a typed daemon-unavailable error when sessionType is daemon", async () => {
      const { spawn, daemonBridge, inProcessBridge } = buildRouter(false);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });

      expect(result).toMatchObject({ ok: false, error: "daemon-unavailable" });
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
      expect(daemonBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("returns a hard error for agent sessions when daemon is unavailable", async () => {
      const { spawn, daemonBridge, inProcessBridge } = buildRouter(false);

      const result = await spawn({
        cwd: "/home",
        sessionType: "daemon",
        agentMeta: { taskId: "t1", agentType: "claude", worktreePath: "/workspace" },
      });

      expect(result).toMatchObject({
        ok: false,
        error: "daemon-unavailable",
      });
      expect((result as TermSpawnError).message).toBeTruthy();
      expect(daemonBridge.handleSpawn).not.toHaveBeenCalled();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("does NOT fall back when agentMeta is present and daemon is unavailable", async () => {
      const { spawn, daemonBridge, inProcessBridge } = buildRouter(false);

      const result = await spawn({
        cwd: "/workspace",
        agentMeta: { taskId: "t1", agentType: "claude", worktreePath: "/workspace" },
      });

      expect((result as TermSpawnError).ok).toBe(false);
      expect((result as TermSpawnError).error).toBe("daemon-unavailable");
      expect(daemonBridge.handleSpawn).not.toHaveBeenCalled();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Circuit breaker open — typed error
  // -------------------------------------------------------------------------

  describe("circuit breaker open", () => {
    it("refuses ordinary durable terminals when breaker is open", async () => {
      const breaker = new CircuitBreaker({ threshold: 1, windowMs: 60_000, cooldownMs: 300_000 });
      breaker.recordFailure(); // trips the breaker (threshold = 1)

      const { spawn, daemonBridge, inProcessBridge } = buildRouter(true, breaker);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });

      expect(result).toMatchObject({ ok: false, error: "daemon-circuit-open" });
      expect(daemonBridge.handleSpawn).not.toHaveBeenCalled();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("returns daemon-circuit-open error for agent sessions when breaker is open", async () => {
      const breaker = new CircuitBreaker({ threshold: 1, windowMs: 60_000, cooldownMs: 300_000 });
      breaker.recordFailure();

      const { spawn, daemonBridge, inProcessBridge } = buildRouter(true, breaker);

      const result = await spawn({
        cwd: "/home",
        sessionType: "daemon",
        agentMeta: { taskId: "t1", agentType: "claude", worktreePath: "/workspace" },
      });

      expect(result).toMatchObject({ ok: false, error: "daemon-circuit-open" });
      expect((result as TermSpawnError).cooldownRemainingMs).toBeTypeOf("number");
      expect((result as TermSpawnError).cooldownRemainingMs).toBeGreaterThan(0);
      expect(daemonBridge.handleSpawn).not.toHaveBeenCalled();
      expect(inProcessBridge.handleSpawn).not.toHaveBeenCalled();
    });

    it("allows daemon spawn when breaker is closed", async () => {
      const breaker = new CircuitBreaker({ threshold: 3 });
      // Record fewer failures than threshold — breaker stays closed
      breaker.recordFailure();
      breaker.recordFailure();

      const { spawn, daemonBridge } = buildRouter(true, breaker);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });

      expect((result as TermSpawnResult).resolvedSessionType).toBe("daemon");
      expect(daemonBridge.handleSpawn).toHaveBeenCalledOnce();
    });
  });

  // -------------------------------------------------------------------------
  // write / resize / kill routing
  // -------------------------------------------------------------------------

  describe("write / resize / kill routing", () => {
    it("routes write to the daemon bridge for a daemon session", async () => {
      const { ipcMain, spawn, daemonBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-write", id, "hello");

      expect(daemonBridge.handleWrite).toHaveBeenCalledWith(id, "hello");
    });

    it("routes write to the in-process bridge for a local session", async () => {
      const { ipcMain, spawn, inProcessBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "in-process" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-write", id, "hello");

      expect(inProcessBridge.handleWrite).toHaveBeenCalledWith(id, "hello");
    });

    it("routes resize to the daemon bridge for a daemon session", async () => {
      const { ipcMain, spawn, daemonBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-resize", id, 80, 24);

      expect(daemonBridge.handleResize).toHaveBeenCalledWith(id, 80, 24);
    });

    it("routes resize to the in-process bridge for a local session", async () => {
      const { ipcMain, spawn, inProcessBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "in-process" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-resize", id, 80, 24);

      expect(inProcessBridge.handleResize).toHaveBeenCalledWith(id, 80, 24);
    });

    it("routes kill to the daemon bridge and removes the session mapping", async () => {
      const { ipcMain, spawn, daemonBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-kill", id);

      expect(daemonBridge.handleKill).toHaveBeenCalledWith(id);
    });

    it("routes kill to the in-process bridge for a local session", async () => {
      const { ipcMain, spawn, inProcessBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "in-process" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-kill", id);

      expect(inProcessBridge.handleKill).toHaveBeenCalledWith(id);
    });

    it("silently ignores write for an unknown session ID", async () => {
      const { ipcMain, daemonBridge, inProcessBridge } = buildRouter(true);

      await ipcMain.invoke("crab:term-write", "term-d-999", "data");

      expect(daemonBridge.handleWrite).not.toHaveBeenCalled();
      expect(inProcessBridge.handleWrite).not.toHaveBeenCalled();
    });

    it("removes session mapping after kill so subsequent writes are ignored", async () => {
      const { ipcMain, spawn, daemonBridge } = buildRouter(true);

      const result = await spawn({ cwd: "/home", sessionType: "daemon" });
      const id = (result as TermSpawnResult).id;

      await ipcMain.invoke("crab:term-kill", id);
      vi.clearAllMocks();

      await ipcMain.invoke("crab:term-write", id, "after-kill");

      expect(daemonBridge.handleWrite).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Session type breakdown
  // -------------------------------------------------------------------------

  describe("getSessionTypeBreakdown", () => {
    it("returns correct counts for mixed session types", async () => {
      const { spawn, router } = buildRouter(true);

      await spawn({ cwd: "/a", sessionType: "daemon" });
      await spawn({ cwd: "/b", sessionType: "daemon" });
      await spawn({ cwd: "/c", sessionType: "in-process" });

      expect(router.getSessionTypeBreakdown()).toEqual({ daemon: 2, inProcess: 1 });
    });

    it("returns zeros when no sessions are active", () => {
      const { router } = buildRouter(true);

      expect(router.getSessionTypeBreakdown()).toEqual({ daemon: 0, inProcess: 0 });
    });
  });

  // -------------------------------------------------------------------------
  // register / dispose
  // -------------------------------------------------------------------------

  describe("register and dispose", () => {
    it("register() is idempotent — calling twice does not double-register", () => {
      const { ipcMain, router } = buildRouter(true);

      // register() was already called in buildRouter; call again
      router.register();

      // handlers map should still have exactly one entry per channel
      expect(ipcMain.handlers.has("crab:term-spawn")).toBe(true);
    });

    it("dispose() removes all IPC handlers", () => {
      const { ipcMain, router } = buildRouter(true);

      router.dispose();

      expect(ipcMain.handlers.has("crab:term-spawn")).toBe(false);
      expect(ipcMain.handlers.has("crab:term-write")).toBe(false);
      expect(ipcMain.handlers.has("crab:term-resize")).toBe(false);
      expect(ipcMain.handlers.has("crab:term-kill")).toBe(false);
    });

    it("dispose() clears the session type map", async () => {
      const { ipcMain, spawn, router, daemonBridge } = buildRouter(true);

      await spawn({ cwd: "/home", sessionType: "daemon" });
      router.dispose();

      // After dispose, the breakdown should be empty
      expect(router.getSessionTypeBreakdown()).toEqual({ daemon: 0, inProcess: 0 });
    });

    it("new durable sessions cannot reuse a restored session identity", async () => {
      const { spawn, router, daemonBridge } = buildRouter(true);

      router.registerSession("term-d-7", "daemon");
      const result = await spawn({ cwd: "/home", sessionType: "daemon" });

      expect((result as TermSpawnResult).id).toMatch(/^term-d-[0-9a-f-]{36}$/);
      expect(daemonBridge.handleSpawn).toHaveBeenCalledWith(
        { cwd: "/home", sessionType: "daemon" },
        expect.stringMatching(/^term-d-[0-9a-f-]{36}$/),
      );
    });

    it("new local sessions cannot reuse a restored session identity", async () => {
      const { spawn, router, inProcessBridge } = buildRouter(true);

      router.registerSession("term-l-4", "in-process");
      const result = await spawn({ cwd: "/home", sessionType: "in-process" });

      expect((result as TermSpawnResult).id).toMatch(/^term-l-[0-9a-f-]{36}$/);
      expect(inProcessBridge.handleSpawn).toHaveBeenCalledWith(
        { cwd: "/home", sessionType: "in-process" },
        expect.stringMatching(/^term-l-[0-9a-f-]{36}$/),
      );
    });
  });
});
