import { describe, it, expect, vi, beforeEach } from "vitest";

import {
  registerDiagnosticsUiIpc,
  type DiagnosticsSnapshot,
  type RestartResult,
} from "../electron/daemon/diagnostics-ui-ipc";
import type { IpcMainLike } from "../electron/daemon/ipc-bridge";
import type { DaemonClient } from "../electron/daemon/daemon-client";
import type { CircuitBreaker } from "../electron/daemon/circuit-breaker";
import type { AutoRespawn } from "../electron/daemon/auto-respawn";
import type { DaemonMetricsSnapshot } from "../electron/daemon/daemon-metrics";

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

function createMockMetrics(): DaemonMetricsSnapshot {
  return {
    uptimeMs: 120_000,
    totalSessionsSpawned: 10,
    activeSessionCount: 3,
    peakSessionCount: 5,
    totalBytesRelayed: 2_048_000,
    totalCheckpointsWritten: 50,
    checkpointFailures: 0,
    clientConnections: 4,
    clientDisconnections: 2,
    respawnCount: 1,
    lastClientConnectedAt: Date.now() - 10_000,
    memoryUsageBytes: 80_000_000,
    fdCount: 64,
  };
}

function createMockClient(metrics?: DaemonMetricsSnapshot | null): DaemonClient {
  const requestFn = metrics === null
    ? vi.fn().mockRejectedValue(new Error("Daemon unreachable"))
    : vi.fn().mockResolvedValue(metrics ?? createMockMetrics());

  return {
    request: requestFn,
  } as unknown as DaemonClient;
}

function createMockCircuitBreaker(state: "closed" | "open" | "half-open" = "closed"): CircuitBreaker {
  return {
    getSnapshot: vi.fn().mockReturnValue({
      state,
      failureCount: state === "open" ? 3 : 0,
      openedAt: state === "open" ? Date.now() - 60_000 : null,
      cooldownRemainingMs: state === "open" ? 240_000 : null,
    }),
  } as unknown as CircuitBreaker;
}

function createMockAutoRespawn(success = true): AutoRespawn {
  return {
    restartDaemon: vi.fn().mockResolvedValue(success),
  } as unknown as AutoRespawn;
}

// ---------------------------------------------------------------------------
// Tests: registerDiagnosticsUiIpc
// ---------------------------------------------------------------------------

describe("registerDiagnosticsUiIpc", () => {
  let ipcMain: ReturnType<typeof createMockIpcMain>;

  beforeEach(() => {
    ipcMain = createMockIpcMain();
  });

  describe("handler registration", () => {
    it("registers crab:daemon-diagnostics handler", () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      expect(ipcMain.handlers.has("crab:daemon-diagnostics")).toBe(true);
    });

    it("registers crab:daemon-restart handler", () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      expect(ipcMain.handlers.has("crab:daemon-restart")).toBe(true);
    });
  });

  describe("crab:daemon-diagnostics", () => {
    it("returns full diagnostics snapshot when daemon is reachable", async () => {
      const metrics = createMockMetrics();
      const client = createMockClient(metrics);
      const circuitBreaker = createMockCircuitBreaker("closed");
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      const handler = ipcMain.handlers.get("crab:daemon-diagnostics")!;
      const result = (await handler()) as DiagnosticsSnapshot;

      expect(result.mode).toBe("daemon");
      expect(result.uptimeMs).toBe(120_000);
      expect(result.activeSessionCount).toBe(3);
      expect(result.memoryUsageBytes).toBe(80_000_000);
      expect(result.fdCount).toBe(64);
      expect(result.protocolVersion).toBe(2);
      expect(result.lastClientConnectedAt).toBe(metrics.lastClientConnectedAt);
      expect(result.circuitBreaker.state).toBe("closed");
      expect(result.totalSessionsSpawned).toBe(10);
      expect(result.peakSessionCount).toBe(5);
      expect(result.respawnCount).toBe(1);
    });

    it("returns null metrics fields when daemon is unreachable", async () => {
      const client = createMockClient(null);
      const circuitBreaker = createMockCircuitBreaker("open");
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      const handler = ipcMain.handlers.get("crab:daemon-diagnostics")!;
      const result = (await handler()) as DiagnosticsSnapshot;

      expect(result.mode).toBe("daemon");
      expect(result.uptimeMs).toBeNull();
      expect(result.activeSessionCount).toBeNull();
      expect(result.memoryUsageBytes).toBeNull();
      expect(result.fdCount).toBeNull();
      expect(result.lastClientConnectedAt).toBeNull();
      expect(result.totalSessionsSpawned).toBeNull();
      expect(result.peakSessionCount).toBeNull();
      expect(result.respawnCount).toBeNull();
      // Protocol version and circuit breaker are always available (local state)
      expect(result.protocolVersion).toBe(2);
      expect(result.circuitBreaker.state).toBe("open");
    });

    it("reports in-process mode from client", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "in-process" });

      const handler = ipcMain.handlers.get("crab:daemon-diagnostics")!;
      const result = (await handler()) as DiagnosticsSnapshot;

      expect(result.mode).toBe("in-process");
    });

    it("includes circuit breaker open state with cooldown info", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker("open");
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      const handler = ipcMain.handlers.get("crab:daemon-diagnostics")!;
      const result = (await handler()) as DiagnosticsSnapshot;

      expect(result.circuitBreaker.state).toBe("open");
      expect(result.circuitBreaker.failureCount).toBe(3);
      expect(result.circuitBreaker.openedAt).not.toBeNull();
      expect(result.circuitBreaker.cooldownRemainingMs).toBe(240_000);
    });

    it("includes circuit breaker half-open state", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker("half-open");
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      const handler = ipcMain.handlers.get("crab:daemon-diagnostics")!;
      const result = (await handler()) as DiagnosticsSnapshot;

      expect(result.circuitBreaker.state).toBe("half-open");
    });

    it("calls client.request with getMetrics", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn();

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      const handler = ipcMain.handlers.get("crab:daemon-diagnostics")!;
      await handler();

      expect(client.request).toHaveBeenCalledWith("getMetrics");
    });
  });

  describe("crab:daemon-restart", () => {
    it("returns success with killed session count on successful restart", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn(true);

      registerDiagnosticsUiIpc({
        ipcMain,
        client,
        circuitBreaker,
        autoRespawn,
        getMode: () => "daemon",
        getActiveSessionCount: () => 4,
      });

      const handler = ipcMain.handlers.get("crab:daemon-restart")!;
      const result = (await handler()) as RestartResult;

      expect(result.success).toBe(true);
      expect(result.killedSessionCount).toBe(4);
      expect(result.error).toBeUndefined();
      expect(autoRespawn.restartDaemon).toHaveBeenCalled();
    });

    it("returns failure when restart fails", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn(false);

      registerDiagnosticsUiIpc({
        ipcMain,
        client,
        circuitBreaker,
        autoRespawn,
        getMode: () => "daemon",
        getActiveSessionCount: () => 2,
      });

      const handler = ipcMain.handlers.get("crab:daemon-restart")!;
      const result = (await handler()) as RestartResult;

      expect(result.success).toBe(false);
      expect(result.killedSessionCount).toBe(2);
      expect(result.error).toBe("Restart failed after maximum retry attempts");
    });

    it("returns failure with error message when restart throws", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = {
        restartDaemon: vi.fn().mockRejectedValue(new Error("Spawner unavailable")),
      } as unknown as AutoRespawn;

      registerDiagnosticsUiIpc({
        ipcMain,
        client,
        circuitBreaker,
        autoRespawn,
        getMode: () => "daemon",
        getActiveSessionCount: () => 1,
      });

      const handler = ipcMain.handlers.get("crab:daemon-restart")!;
      const result = (await handler()) as RestartResult;

      expect(result.success).toBe(false);
      expect(result.killedSessionCount).toBe(0);
      expect(result.error).toBe("Spawner unavailable");
    });

    it("reports zero killed sessions when getActiveSessionCount is not provided", async () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn(true);

      registerDiagnosticsUiIpc({ ipcMain, client, circuitBreaker, autoRespawn, getMode: () => "daemon" });

      const handler = ipcMain.handlers.get("crab:daemon-restart")!;
      const result = (await handler()) as RestartResult;

      expect(result.success).toBe(true);
      expect(result.killedSessionCount).toBe(0);
    });
  });

  describe("dispose", () => {
    it("removes both IPC handlers", () => {
      const client = createMockClient();
      const circuitBreaker = createMockCircuitBreaker();
      const autoRespawn = createMockAutoRespawn();

      const { dispose } = registerDiagnosticsUiIpc({
        ipcMain,
        client,
        circuitBreaker,
        autoRespawn,
        getMode: () => "daemon",
      });

      expect(ipcMain.handlers.has("crab:daemon-diagnostics")).toBe(true);
      expect(ipcMain.handlers.has("crab:daemon-restart")).toBe(true);

      dispose();

      expect(ipcMain.handlers.has("crab:daemon-diagnostics")).toBe(false);
      expect(ipcMain.handlers.has("crab:daemon-restart")).toBe(false);
    });
  });
});
