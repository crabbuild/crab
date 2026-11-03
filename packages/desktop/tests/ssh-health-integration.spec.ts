/**
 * Integration tests for the SSH health monitoring pipeline.
 *
 * Verifies the end-to-end flow from a mock SSH channel through the
 * real RpcRouter to the SystemHealthMonitor. Tests cover:
 * - Linux server with GPU (full health response)
 * - Linux server without GPU (gpus: null)
 * - Full pipeline: mock SSH channel → real RpcRouter → health monitor
 * - Alert generation (disk >90%, GPU temp >85°C)
 * - Bandwidth tracking during health requests
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { EventEmitter } from "node:events";
import { RpcRouter } from "../electron/ssh-router";
import { SystemHealthMonitor, type SystemHealth, type HealthAlert } from "../electron/ssh-health";

// ── Mock SSH Channel ─────────────────────────────────────────────

class MockSshChannel extends EventEmitter {
  written: string[] = [];

  write(data: string, cb?: (err?: Error | null) => void): boolean {
    this.written.push(data);
    cb?.(null);
    return true;
  }

  end = vi.fn();
}

// ── Mock Session Manager ─────────────────────────────────────────

class MockSessionManager extends EventEmitter {
  private statuses = new Map<string, string>();
  private connections = new Map<string, { channels: { agent?: MockSshChannel } }>();

  setStatus(profileId: string, status: string): void {
    this.statuses.set(profileId, status);
  }

  setConnection(profileId: string, channel: MockSshChannel): void {
    this.connections.set(profileId, { channels: { agent: channel } });
  }

  getStatus(profileId: string): string {
    return this.statuses.get(profileId) ?? "disconnected";
  }

  getConnection(profileId: string): { channels: { agent?: MockSshChannel } } | undefined {
    return this.connections.get(profileId);
  }
}

// ── Realistic health responses ───────────────────────────────────

function makeLinuxGpuHealthResponse(): SystemHealth {
  return {
    os: "Linux",
    kernel: "5.15.0-91-generic",
    arch: "x86_64",
    hostname: "gpu-training-01",
    uptime: 1_209_600, // 14 days
    loadAvg: [3.2, 2.8, 2.1],
    disk: [
      { path: "/", total: 500_000_000_000, free: 200_000_000_000, usedPercent: 60 },
      { path: "/data", total: 4_000_000_000_000, free: 2_800_000_000_000, usedPercent: 30 },
      { path: "/scratch", total: 1_000_000_000_000, free: 100_000_000_000, usedPercent: 90 },
    ],
    memory: { total: 256_000_000_000, free: 64_000_000_000, cached: 96_000_000_000, usedPercent: 75 },
    gpus: [
      {
        index: 0,
        name: "NVIDIA A100-SXM4-80GB",
        utilization: 95,
        memoryUsed: 72_000_000_000,
        memoryTotal: 85_899_345_920,
        temperature: 72,
        processes: [
          { pid: 12345, name: "python3", memUsed: 68_000_000_000 },
          { pid: 12346, name: "tensorboard", memUsed: 2_000_000_000 },
        ],
      },
      {
        index: 1,
        name: "NVIDIA A100-SXM4-80GB",
        utilization: 88,
        memoryUsed: 65_000_000_000,
        memoryTotal: 85_899_345_920,
        temperature: 70,
        processes: [
          { pid: 12347, name: "python3", memUsed: 64_000_000_000 },
        ],
      },
    ],
    collectedAt: Date.now(),
  };
}

function makeLinuxNoGpuHealthResponse(): SystemHealth {
  return {
    os: "Linux",
    kernel: "6.1.0-18-amd64",
    arch: "x86_64",
    hostname: "build-server-03",
    uptime: 604_800, // 7 days
    loadAvg: [0.5, 0.3, 0.2],
    disk: [
      { path: "/", total: 250_000_000_000, free: 180_000_000_000, usedPercent: 28 },
      { path: "/home", total: 1_000_000_000_000, free: 700_000_000_000, usedPercent: 30 },
    ],
    memory: { total: 32_000_000_000, free: 16_000_000_000, cached: 8_000_000_000, usedPercent: 50 },
    gpus: null as any,
    collectedAt: Date.now(),
  };
}

function makeAlertTriggeringHealthResponse(): SystemHealth {
  return {
    os: "Linux",
    kernel: "5.15.0-91-generic",
    arch: "x86_64",
    hostname: "overloaded-server",
    uptime: 86_400,
    loadAvg: [12.5, 10.2, 8.7],
    disk: [
      { path: "/", total: 500_000_000_000, free: 25_000_000_000, usedPercent: 95 },
    ],
    memory: { total: 128_000_000_000, free: 5_000_000_000, cached: 3_000_000_000, usedPercent: 94 },
    gpus: [
      {
        index: 0,
        name: "NVIDIA A100",
        utilization: 100,
        memoryUsed: 80_000_000_000,
        memoryTotal: 85_899_345_920,
        temperature: 88,
        processes: [{ pid: 9999, name: "train.py", memUsed: 79_000_000_000 }],
      },
      {
        index: 1,
        name: "NVIDIA A100",
        utilization: 100,
        memoryUsed: 80_000_000_000,
        memoryTotal: 85_899_345_920,
        temperature: 92,
        processes: [{ pid: 9998, name: "train.py", memUsed: 79_000_000_000 }],
      },
    ],
    collectedAt: Date.now(),
  };
}

// ── Helpers ──────────────────────────────────────────────────────

/**
 * Set up a full pipeline: MockSshChannel → RpcRouter → SystemHealthMonitor.
 * The channel auto-responds to system_health RPC requests with the given response.
 */
function setupPipeline(healthResponse: SystemHealth, profileId = "gpu-server") {
  const sessionManager = new MockSessionManager();
  const channel = new MockSshChannel();
  const localAgent = { request: vi.fn().mockResolvedValue(null) };

  sessionManager.setStatus(profileId, "connected");
  sessionManager.setConnection(profileId, channel);

  const router = new RpcRouter(sessionManager as any, localAgent, { requestTimeoutMs: 5000 });
  router.attachChannel(profileId, channel as any);

  // Auto-respond to system_health RPC requests on the channel
  channel.on("data-written", () => {});
  const originalWrite = channel.write.bind(channel);
  channel.write = (data: string, cb?: (err?: Error | null) => void) => {
    originalWrite(data, cb);
    // Parse the request and respond with health data
    try {
      const request = JSON.parse(data.trim());
      if (request.method === "system_health") {
        const response = JSON.stringify({
          jsonrpc: "2.0",
          id: request.id,
          result: healthResponse,
        }) + "\n";
        // Simulate async response from remote agent
        setImmediate(() => {
          channel.emit("data", Buffer.from(response));
        });
      }
    } catch {
      // Ignore parse errors for non-JSON data
    }
    return true;
  };

  const monitor = new SystemHealthMonitor(router as any, sessionManager as any, 1000);

  return { sessionManager, channel, router, monitor, localAgent };
}

// ── Integration Tests ────────────────────────────────────────────

describe("SSH Health Integration — Linux server with GPU", () => {
  let monitor: SystemHealthMonitor;
  let cleanup: () => void;

  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    monitor?.stopAll();
    vi.useRealTimers();
  });

  it("collects and parses full health data from a GPU server", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData);
    monitor = m;

    const updates: Array<{ profileId: string; health: SystemHealth }> = [];
    monitor.on("ssh:health-update", (payload) => updates.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");
    await vi.advanceTimersByTimeAsync(0);

    expect(updates).toHaveLength(1);
    const health = updates[0].health;

    // Verify all fields are correctly parsed
    expect(health.os).toBe("Linux");
    expect(health.kernel).toBe("5.15.0-91-generic");
    expect(health.arch).toBe("x86_64");
    expect(health.hostname).toBe("gpu-training-01");
    expect(health.uptime).toBe(1_209_600);
    expect(health.loadAvg).toEqual([3.2, 2.8, 2.1]);

    // Disk info
    expect(health.disk).toHaveLength(3);
    expect(health.disk[0].path).toBe("/");
    expect(health.disk[0].usedPercent).toBe(60);
    expect(health.disk[2].path).toBe("/scratch");
    expect(health.disk[2].usedPercent).toBe(90);

    // Memory info
    expect(health.memory.total).toBe(256_000_000_000);
    expect(health.memory.usedPercent).toBe(75);

    // GPU info with processes
    expect(health.gpus).toHaveLength(2);
    expect(health.gpus![0].name).toBe("NVIDIA A100-SXM4-80GB");
    expect(health.gpus![0].utilization).toBe(95);
    expect(health.gpus![0].temperature).toBe(72);
    expect(health.gpus![0].processes).toHaveLength(2);
    expect(health.gpus![0].processes[0].pid).toBe(12345);
    expect(health.gpus![0].processes[0].name).toBe("python3");
    expect(health.gpus![1].utilization).toBe(88);

    // collectedAt is set
    expect(health.collectedAt).toBeGreaterThan(0);
  });

  it("formatSummary includes GPU info for a GPU server", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData);
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");
    await vi.advanceTimersByTimeAsync(0);

    const health = monitor.getLastHealth("gpu-server")!;
    const summary = monitor.formatSummary(health);

    expect(summary).toContain("A100-SXM4-80GB");
    expect(summary).toContain("2x");
    expect(summary).toContain("util");
    expect(summary).toContain("RAM");
    expect(summary).toContain("free");
  });
});

describe("SSH Health Integration — Linux server without GPU", () => {
  let monitor: SystemHealthMonitor;

  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    monitor?.stopAll();
    vi.useRealTimers();
  });

  it("handles gpus: null gracefully", async () => {
    const healthData = makeLinuxNoGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "build-server");
    monitor = m;

    const updates: Array<{ profileId: string; health: SystemHealth }> = [];
    monitor.on("ssh:health-update", (payload) => updates.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("build-server");
    await vi.advanceTimersByTimeAsync(0);

    expect(updates).toHaveLength(1);
    const health = updates[0].health;

    expect(health.os).toBe("Linux");
    expect(health.hostname).toBe("build-server-03");
    expect(health.gpus).toBeNull();
    expect(health.disk).toHaveLength(2);
    expect(health.memory.usedPercent).toBe(50);
  });

  it("formatSummary does not include GPU info when gpus is null", async () => {
    const healthData = makeLinuxNoGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "build-server");
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("build-server");
    await vi.advanceTimersByTimeAsync(0);

    const health = monitor.getLastHealth("build-server")!;
    const summary = monitor.formatSummary(health);

    // Should not contain GPU-related text
    expect(summary).not.toContain("util");
    expect(summary).not.toContain("NVIDIA");
    expect(summary).not.toContain("A100");

    // Should still contain disk and memory info
    expect(summary).toContain("free");
    expect(summary).toContain("RAM");
  });

  it("does not emit GPU temperature alerts when no GPUs present", async () => {
    const healthData = makeLinuxNoGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "build-server");
    monitor = m;

    const alerts: Array<{ profileId: string; alert: HealthAlert }> = [];
    monitor.on("health:alert", (payload) => alerts.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("build-server");
    await vi.advanceTimersByTimeAsync(0);

    const gpuAlerts = alerts.filter((a) => a.alert.type === "gpu_temp");
    expect(gpuAlerts).toHaveLength(0);
  });
});

describe("SSH Health Integration — full pipeline with real RpcRouter", () => {
  let monitor: SystemHealthMonitor;

  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    monitor?.stopAll();
    vi.useRealTimers();
  });

  it("routes system_health RPC through the real router to the mock channel", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m, channel } = setupPipeline(healthData);
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");
    await vi.advanceTimersByTimeAsync(0);

    // Verify the channel received a properly formatted JSON-RPC request
    expect(channel.written).toHaveLength(1);
    const request = JSON.parse(channel.written[0].trim());
    expect(request.jsonrpc).toBe("2.0");
    expect(request.method).toBe("system_health");
    expect(request.params).toEqual({ include_gpu: true });
    expect(request.id).toBeGreaterThan(0);
  });

  it("handles the full request-response cycle through the router", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData);
    monitor = m;

    // refresh() triggers pollHealth which uses setImmediate for the mock response
    const resultPromise = monitor.refresh("gpu-server");
    await vi.advanceTimersByTimeAsync(0);
    const result = await resultPromise;

    expect(result.os).toBe("Linux");
    expect(result.hostname).toBe("gpu-training-01");
    expect(result.gpus).toHaveLength(2);
    expect(result.collectedAt).toBeGreaterThan(0);
  });

  it("caches health data after successful poll", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m } = setupPipeline(healthData);
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");
    await vi.advanceTimersByTimeAsync(0);

    const cached = monitor.getLastHealth("gpu-server");
    expect(cached).toBeDefined();
    expect(cached!.hostname).toBe("gpu-training-01");
    expect(cached!.gpus).toHaveLength(2);
  });

  it("polls at the configured interval through the full pipeline", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m, channel } = setupPipeline(healthData);
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");

    // Immediate poll
    await vi.advanceTimersByTimeAsync(0);
    expect(channel.written).toHaveLength(1);

    // After one interval (1000ms configured in setupPipeline)
    await vi.advanceTimersByTimeAsync(1000);
    expect(channel.written).toHaveLength(2);

    // After another interval
    await vi.advanceTimersByTimeAsync(1000);
    expect(channel.written).toHaveLength(3);
  });
});

describe("SSH Health Integration — alert generation", () => {
  let monitor: SystemHealthMonitor;

  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    monitor?.stopAll();
    vi.useRealTimers();
  });

  it("emits disk critical alert when usage exceeds 90%", async () => {
    const healthData = makeAlertTriggeringHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "overloaded");
    monitor = m;

    const alerts: Array<{ profileId: string; alert: HealthAlert }> = [];
    monitor.on("health:alert", (payload) => alerts.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("overloaded");
    await vi.advanceTimersByTimeAsync(0);

    const diskAlerts = alerts.filter((a) => a.alert.type === "disk");
    expect(diskAlerts).toHaveLength(1);
    expect(diskAlerts[0].alert.severity).toBe("critical");
    expect(diskAlerts[0].alert.value).toBe(95);
    expect(diskAlerts[0].profileId).toBe("overloaded");
  });

  it("emits GPU temperature alerts for overheating GPUs", async () => {
    const healthData = makeAlertTriggeringHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "overloaded");
    monitor = m;

    const alerts: Array<{ profileId: string; alert: HealthAlert }> = [];
    monitor.on("health:alert", (payload) => alerts.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("overloaded");
    await vi.advanceTimersByTimeAsync(0);

    const gpuAlerts = alerts.filter((a) => a.alert.type === "gpu_temp");
    expect(gpuAlerts).toHaveLength(2);

    // GPU 0 at 88°C → warning
    expect(gpuAlerts[0].alert.severity).toBe("warning");
    expect(gpuAlerts[0].alert.value).toBe(88);

    // GPU 1 at 92°C → critical
    expect(gpuAlerts[1].alert.severity).toBe("critical");
    expect(gpuAlerts[1].alert.value).toBe(92);
  });

  it("emits memory warning when usage exceeds 90%", async () => {
    const healthData = makeAlertTriggeringHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "overloaded");
    monitor = m;

    const alerts: Array<{ profileId: string; alert: HealthAlert }> = [];
    monitor.on("health:alert", (payload) => alerts.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("overloaded");
    await vi.advanceTimersByTimeAsync(0);

    const memAlerts = alerts.filter((a) => a.alert.type === "memory");
    expect(memAlerts).toHaveLength(1);
    expect(memAlerts[0].alert.severity).toBe("warning");
    expect(memAlerts[0].alert.value).toBe(94);
  });

  it("emits alerts-snapshot with all current alerts", async () => {
    const healthData = makeAlertTriggeringHealthResponse();
    const { monitor: m } = setupPipeline(healthData, "overloaded");
    monitor = m;

    const snapshots: Array<{ profileId: string; alerts: HealthAlert[] }> = [];
    monitor.on("health:alerts-snapshot", (payload) => snapshots.push(payload));

    monitor.setVisible(true);
    monitor.startPolling("overloaded");
    await vi.advanceTimersByTimeAsync(0);

    expect(snapshots).toHaveLength(1);
    // Should contain disk + memory + 2 GPU temp alerts = 4 total
    expect(snapshots[0].alerts.length).toBeGreaterThanOrEqual(4);

    const types = snapshots[0].alerts.map((a) => a.type);
    expect(types).toContain("disk");
    expect(types).toContain("memory");
    expect(types).toContain("gpu_temp");
  });
});

describe("SSH Health Integration — bandwidth tracking", () => {
  let monitor: SystemHealthMonitor;

  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    monitor?.stopAll();
    vi.useRealTimers();
  });

  it("updates bandwidth counters when health request is sent", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m, router } = setupPipeline(healthData);
    monitor = m;

    // Bandwidth should start at zero
    const before = router.getBandwidth("gpu-server");
    expect(before.bytesOut).toBe(0);
    expect(before.bytesIn).toBe(0);

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");
    await vi.advanceTimersByTimeAsync(0);

    const after = router.getBandwidth("gpu-server");
    // Outgoing: the JSON-RPC request was sent
    expect(after.bytesOut).toBeGreaterThan(0);
    // Incoming: the JSON-RPC response was received
    expect(after.bytesIn).toBeGreaterThan(0);
  });

  it("accumulates bandwidth across multiple health polls", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m, router } = setupPipeline(healthData);
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");

    // First poll
    await vi.advanceTimersByTimeAsync(0);
    const afterFirst = { ...router.getBandwidth("gpu-server") };
    expect(afterFirst.bytesOut).toBeGreaterThan(0);
    expect(afterFirst.bytesIn).toBeGreaterThan(0);

    // Second poll (advance past the interval)
    await vi.advanceTimersByTimeAsync(1000);
    await vi.advanceTimersByTimeAsync(0);
    const afterSecond = router.getBandwidth("gpu-server");

    // Outgoing bytes should increase (request is written synchronously)
    expect(afterSecond.bytesOut).toBeGreaterThan(afterFirst.bytesOut);
    // Total incoming should be at least what we had after the first poll
    expect(afterSecond.bytesIn).toBeGreaterThanOrEqual(afterFirst.bytesIn);
  });

  it("bandwidth reflects realistic payload sizes", async () => {
    const healthData = makeLinuxGpuHealthResponse();
    const { monitor: m, router } = setupPipeline(healthData);
    monitor = m;

    monitor.setVisible(true);
    monitor.startPolling("gpu-server");
    await vi.advanceTimersByTimeAsync(0);

    const stats = router.getBandwidth("gpu-server");

    // The request is a small JSON-RPC frame (~80-100 bytes)
    expect(stats.bytesOut).toBeGreaterThan(50);
    expect(stats.bytesOut).toBeLessThan(200);

    // The response contains full health data with GPUs (~800-1500 bytes)
    expect(stats.bytesIn).toBeGreaterThan(500);
    expect(stats.bytesIn).toBeLessThan(3000);
  });
});
