import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

import {
  DaemonMetrics,
  getFdCount,
  type DaemonMetricsSnapshot,
} from "../electron/daemon/daemon-metrics";
import {
  registerDaemonMetricsIpc,
} from "../electron/daemon/daemon-metrics-ipc";
import type { IpcMainLike } from "../electron/daemon/ipc-bridge";
import type { DaemonClient } from "../electron/daemon/daemon-client";

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

function createMockClient(metricsResponse?: DaemonMetricsSnapshot): DaemonClient {
  return {
    request: vi.fn().mockResolvedValue(metricsResponse ?? null),
  } as unknown as DaemonClient;
}

// ---------------------------------------------------------------------------
// Tests: DaemonMetrics
// ---------------------------------------------------------------------------

describe("DaemonMetrics", () => {
  let metrics: DaemonMetrics;

  afterEach(() => {
    metrics?.dispose();
  });

  describe("constructor and initial state", () => {
    it("starts with zero counters", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      const snapshot = metrics.getMetrics();

      expect(snapshot.totalSessionsSpawned).toBe(0);
      expect(snapshot.activeSessionCount).toBe(0);
      expect(snapshot.peakSessionCount).toBe(0);
      expect(snapshot.totalBytesRelayed).toBe(0);
      expect(snapshot.totalCheckpointsWritten).toBe(0);
      expect(snapshot.checkpointFailures).toBe(0);
      expect(snapshot.clientConnections).toBe(0);
      expect(snapshot.clientDisconnections).toBe(0);
      expect(snapshot.respawnCount).toBe(0);
      expect(snapshot.lastClientConnectedAt).toBeNull();
    });

    it("reports uptime based on elapsed time", async () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      // Wait a small amount to ensure uptime > 0
      await new Promise((r) => setTimeout(r, 10));
      const snapshot = metrics.getMetrics();
      expect(snapshot.uptimeMs).toBeGreaterThanOrEqual(10);
    });

    it("reports memory usage from process.memoryUsage", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      const snapshot = metrics.getMetrics();
      expect(snapshot.memoryUsageBytes).toBeGreaterThan(0);
    });
  });

  describe("recordSessionSpawned", () => {
    it("increments totalSessionsSpawned", () => {
      let sessionCount = 0;
      metrics = new DaemonMetrics({
        fdPollIntervalMs: 0,
        getActiveSessionCount: () => sessionCount,
      });

      sessionCount = 1;
      metrics.recordSessionSpawned();
      expect(metrics.getMetrics().totalSessionsSpawned).toBe(1);

      sessionCount = 2;
      metrics.recordSessionSpawned();
      expect(metrics.getMetrics().totalSessionsSpawned).toBe(2);
    });

    it("updates peakSessionCount when active count exceeds previous peak", () => {
      let sessionCount = 0;
      metrics = new DaemonMetrics({
        fdPollIntervalMs: 0,
        getActiveSessionCount: () => sessionCount,
      });

      sessionCount = 3;
      metrics.recordSessionSpawned();
      expect(metrics.getMetrics().peakSessionCount).toBe(3);

      // Session count drops, peak should remain
      sessionCount = 1;
      metrics.recordSessionSpawned();
      expect(metrics.getMetrics().peakSessionCount).toBe(3);

      // Session count exceeds peak
      sessionCount = 5;
      metrics.recordSessionSpawned();
      expect(metrics.getMetrics().peakSessionCount).toBe(5);
    });
  });

  describe("recordBytesRelayed", () => {
    it("accumulates total bytes relayed", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });

      metrics.recordBytesRelayed(1024);
      metrics.recordBytesRelayed(2048);
      metrics.recordBytesRelayed(512);

      expect(metrics.getMetrics().totalBytesRelayed).toBe(3584);
    });
  });

  describe("recordCheckpointWritten / recordCheckpointFailure", () => {
    it("tracks checkpoint successes and failures independently", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });

      metrics.recordCheckpointWritten();
      metrics.recordCheckpointWritten();
      metrics.recordCheckpointFailure();

      const snapshot = metrics.getMetrics();
      expect(snapshot.totalCheckpointsWritten).toBe(2);
      expect(snapshot.checkpointFailures).toBe(1);
    });
  });

  describe("recordClientConnected / recordClientDisconnected", () => {
    it("tracks client connections and disconnections", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });

      metrics.recordClientConnected();
      metrics.recordClientConnected();
      metrics.recordClientDisconnected();

      const snapshot = metrics.getMetrics();
      expect(snapshot.clientConnections).toBe(2);
      expect(snapshot.clientDisconnections).toBe(1);
    });

    it("updates lastClientConnectedAt on connection", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });

      expect(metrics.getMetrics().lastClientConnectedAt).toBeNull();

      const before = Date.now();
      metrics.recordClientConnected();
      const after = Date.now();

      const lastConnected = metrics.getMetrics().lastClientConnectedAt;
      expect(lastConnected).toBeGreaterThanOrEqual(before);
      expect(lastConnected).toBeLessThanOrEqual(after);
    });
  });

  describe("recordRespawn", () => {
    it("increments respawn count", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });

      metrics.recordRespawn();
      metrics.recordRespawn();

      expect(metrics.getMetrics().respawnCount).toBe(2);
    });
  });

  describe("updatePeakSessionCount", () => {
    it("updates peak when active count exceeds previous peak", () => {
      let sessionCount = 0;
      metrics = new DaemonMetrics({
        fdPollIntervalMs: 0,
        getActiveSessionCount: () => sessionCount,
      });

      sessionCount = 4;
      metrics.updatePeakSessionCount();
      expect(metrics.getMetrics().peakSessionCount).toBe(4);

      sessionCount = 2;
      metrics.updatePeakSessionCount();
      expect(metrics.getMetrics().peakSessionCount).toBe(4);
    });
  });

  describe("activeSessionCount", () => {
    it("delegates to getActiveSessionCount callback", () => {
      let sessionCount = 7;
      metrics = new DaemonMetrics({
        fdPollIntervalMs: 0,
        getActiveSessionCount: () => sessionCount,
      });

      expect(metrics.getMetrics().activeSessionCount).toBe(7);

      sessionCount = 3;
      expect(metrics.getMetrics().activeSessionCount).toBe(3);
    });

    it("defaults to 0 when no callback provided", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      expect(metrics.getMetrics().activeSessionCount).toBe(0);
    });
  });

  describe("fd count polling", () => {
    it("polls fd count on construction", async () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      // Give the async poll time to complete
      await new Promise((r) => setTimeout(r, 100));

      const fdCount = metrics.getFdCount();
      // On Linux/macOS, should be a positive number; on other platforms, -1
      if (process.platform === "linux" || process.platform === "darwin") {
        expect(fdCount).toBeGreaterThan(0);
      } else {
        expect(fdCount).toBe(-1);
      }
    });

    it("includes fdCount in metrics snapshot", async () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      await new Promise((r) => setTimeout(r, 100));

      const snapshot = metrics.getMetrics();
      expect(snapshot.fdCount).toBe(metrics.getFdCount());
    });
  });

  describe("dispose", () => {
    it("stops the fd poll timer", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 1000 });
      metrics.dispose();
      // Calling dispose again should be safe (idempotent)
      metrics.dispose();
    });

    it("is safe to call getMetrics after dispose", () => {
      metrics = new DaemonMetrics({ fdPollIntervalMs: 0 });
      metrics.dispose();
      // Should not throw
      const snapshot = metrics.getMetrics();
      expect(snapshot.uptimeMs).toBeGreaterThanOrEqual(0);
    });
  });
});

// ---------------------------------------------------------------------------
// Tests: getFdCount
// ---------------------------------------------------------------------------

describe("getFdCount", () => {
  it("returns a positive number on Linux or macOS", async () => {
    const count = await getFdCount();
    if (process.platform === "linux" || process.platform === "darwin") {
      expect(count).toBeGreaterThan(0);
    } else {
      expect(count).toBe(-1);
    }
  });
});

// ---------------------------------------------------------------------------
// Tests: registerDaemonMetricsIpc
// ---------------------------------------------------------------------------

describe("registerDaemonMetricsIpc", () => {
  let ipcMain: ReturnType<typeof createMockIpcMain>;

  beforeEach(() => {
    ipcMain = createMockIpcMain();
  });

  it("registers the crab:daemon-metrics IPC handler", () => {
    const client = createMockClient();
    registerDaemonMetricsIpc({ ipcMain, client });
    expect(ipcMain.handlers.has("crab:daemon-metrics")).toBe(true);
  });

  it("returns metrics snapshot from daemon", async () => {
    const mockMetrics: DaemonMetricsSnapshot = {
      uptimeMs: 60000,
      totalSessionsSpawned: 5,
      activeSessionCount: 2,
      peakSessionCount: 4,
      totalBytesRelayed: 1024000,
      totalCheckpointsWritten: 100,
      checkpointFailures: 1,
      clientConnections: 3,
      clientDisconnections: 1,
      respawnCount: 0,
      lastClientConnectedAt: Date.now() - 5000,
      memoryUsageBytes: 50_000_000,
      fdCount: 42,
    };

    const client = createMockClient(mockMetrics);
    registerDaemonMetricsIpc({ ipcMain, client });

    const handler = ipcMain.handlers.get("crab:daemon-metrics")!;
    const result = await handler();

    expect(result).toEqual(mockMetrics);
    expect(client.request).toHaveBeenCalledWith("getMetrics");
  });

  it("returns null when daemon request fails", async () => {
    const client = {
      request: vi.fn().mockRejectedValue(new Error("Daemon unreachable")),
    } as unknown as DaemonClient;

    registerDaemonMetricsIpc({ ipcMain, client });

    const handler = ipcMain.handlers.get("crab:daemon-metrics")!;
    const result = await handler();

    expect(result).toBeNull();
  });

  describe("dispose", () => {
    it("removes the crab:daemon-metrics handler", () => {
      const client = createMockClient();
      const { dispose } = registerDaemonMetricsIpc({ ipcMain, client });

      expect(ipcMain.handlers.has("crab:daemon-metrics")).toBe(true);
      dispose();
      expect(ipcMain.handlers.has("crab:daemon-metrics")).toBe(false);
    });
  });
});
