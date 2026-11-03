/**
 * Unit tests for the SystemHealthMonitor class.
 *
 * Covers: polling start/stop, health data parsing and emission,
 * visibility-based polling pause/resume, error handling, and cache behavior.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { SystemHealthMonitor, type SystemHealth } from "../electron/ssh-health";

// ── Test helpers ─────────────────────────────────────────────────

function makeMockRouter(response?: unknown, shouldReject?: boolean) {
  const route = shouldReject
    ? vi.fn().mockRejectedValue(response ?? new Error("RPC failed"))
    : vi.fn().mockResolvedValue(response ?? makeHealthResponse());

  return { route, batchRpc: vi.fn() } as any;
}

function makeMockSessionManager() {
  return {} as any;
}

function makeHealthResponse(overrides?: Partial<SystemHealth>): SystemHealth {
  return {
    os: "Linux",
    kernel: "6.1.0",
    arch: "x86_64",
    hostname: "gpu-server-01",
    uptime: 86400,
    loadAvg: [1.2, 0.8, 0.5] as [number, number, number],
    disk: [
      { path: "/", total: 500_000_000_000, free: 200_000_000_000, usedPercent: 60 },
      { path: "/data", total: 2_000_000_000_000, free: 1_800_000_000_000, usedPercent: 10 },
    ],
    memory: { total: 128_000_000_000, free: 32_000_000_000, cached: 48_000_000_000, usedPercent: 75 },
    gpus: [
      {
        index: 0,
        name: "NVIDIA A100",
        utilization: 87,
        memoryUsed: 30_000_000_000,
        memoryTotal: 40_000_000_000,
        temperature: 72,
        processes: [{ pid: 1234, name: "python", memUsed: 28_000_000_000 }],
      },
    ],
    collectedAt: Date.now(),
    ...overrides,
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("SystemHealthMonitor", () => {
  let monitor: SystemHealthMonitor;
  let mockRouter: ReturnType<typeof makeMockRouter>;

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRouter();
    monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);
  });

  afterEach(() => {
    monitor.stopAll();
    vi.useRealTimers();
  });

  describe("polling start/stop", () => {
    it("starts polling and performs an immediate fetch when visible", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      // Flush the immediate poll (microtask for the async call)
      await vi.advanceTimersByTimeAsync(0);

      expect(mockRouter.route).toHaveBeenCalledWith(
        "ssh://profile-1/",
        "system_health",
        { include_gpu: true },
      );
    });

    it("does not poll when not visible", async () => {
      monitor.startPolling("profile-1");

      await vi.advanceTimersByTimeAsync(5000);

      expect(mockRouter.route).not.toHaveBeenCalled();
    });

    it("stops polling when stopPolling is called", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.route).toHaveBeenCalledTimes(1);

      monitor.stopPolling("profile-1");
      mockRouter.route.mockClear();

      // Advance past the poll interval
      await vi.advanceTimersByTimeAsync(2000);

      expect(mockRouter.route).not.toHaveBeenCalled();
    });

    it("polls at the configured interval", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      // Immediate poll
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.route).toHaveBeenCalledTimes(1);

      // After one interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.route).toHaveBeenCalledTimes(2);

      // After another interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.route).toHaveBeenCalledTimes(3);
    });

    it("does not start duplicate intervals for the same profile", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      monitor.startPolling("profile-1");

      await vi.advanceTimersByTimeAsync(0);

      // Only one immediate poll, not two
      expect(mockRouter.route).toHaveBeenCalledTimes(1);
    });

    it("stopAll clears all profiles and intervals", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      monitor.startPolling("profile-2");

      await vi.advanceTimersByTimeAsync(0);
      // Two immediate polls (one per profile)
      expect(mockRouter.route).toHaveBeenCalledTimes(2);

      monitor.stopAll();
      mockRouter.route.mockClear();

      await vi.advanceTimersByTimeAsync(5000);
      expect(mockRouter.route).not.toHaveBeenCalled();
      expect(monitor.getActiveProfiles().size).toBe(0);
    });

    it("isPolling returns correct state", async () => {
      monitor.setVisible(true);
      expect(monitor.isPolling("profile-1")).toBe(false);

      monitor.startPolling("profile-1");
      expect(monitor.isPolling("profile-1")).toBe(true);

      monitor.stopPolling("profile-1");
      expect(monitor.isPolling("profile-1")).toBe(false);
    });
  });

  describe("health data parsing and emission", () => {
    it("emits ssh:health-update with parsed health data", async () => {
      const healthData = makeHealthResponse();
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const updates: Array<{ profileId: string; health: SystemHealth }> = [];
      monitor.on("ssh:health-update", (payload) => updates.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(updates).toHaveLength(1);
      expect(updates[0].profileId).toBe("profile-1");
      expect(updates[0].health.os).toBe("Linux");
      expect(updates[0].health.kernel).toBe("6.1.0");
      expect(updates[0].health.arch).toBe("x86_64");
      expect(updates[0].health.hostname).toBe("gpu-server-01");
      expect(updates[0].health.uptime).toBe(86400);
      expect(updates[0].health.loadAvg).toEqual([1.2, 0.8, 0.5]);
      expect(updates[0].health.disk).toHaveLength(2);
      expect(updates[0].health.memory.total).toBe(128_000_000_000);
      expect(updates[0].health.gpus).toHaveLength(1);
      expect(updates[0].health.gpus![0].name).toBe("NVIDIA A100");
      expect(updates[0].health.collectedAt).toBeGreaterThan(0);
    });

    it("sets collectedAt timestamp on each poll", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      const health = monitor.getLastHealth("profile-1");
      expect(health).toBeDefined();
      expect(health!.collectedAt).toBeGreaterThan(0);
    });

    it("sends system_health RPC with include_gpu param", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(mockRouter.route).toHaveBeenCalledWith(
        "ssh://profile-1/",
        "system_health",
        { include_gpu: true },
      );
    });
  });

  describe("visibility control", () => {
    it("pauses polling when setVisible(false) is called", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.route).toHaveBeenCalledTimes(1);

      monitor.setVisible(false);
      mockRouter.route.mockClear();

      await vi.advanceTimersByTimeAsync(5000);
      expect(mockRouter.route).not.toHaveBeenCalled();
    });

    it("resumes polling when setVisible(true) is called after being hidden", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      monitor.setVisible(false);
      mockRouter.route.mockClear();

      monitor.setVisible(true);
      await vi.advanceTimersByTimeAsync(0);

      // Should have polled again on resume
      expect(mockRouter.route).toHaveBeenCalled();
    });

    it("resumes polling for all active profiles on visibility restore", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      monitor.startPolling("profile-2");
      await vi.advanceTimersByTimeAsync(0);

      monitor.setVisible(false);
      mockRouter.route.mockClear();

      monitor.setVisible(true);
      await vi.advanceTimersByTimeAsync(0);

      // Both profiles should be polled
      const calledProfiles = mockRouter.route.mock.calls.map(
        (call: unknown[]) => call[0],
      );
      expect(calledProfiles).toContain("ssh://profile-1/");
      expect(calledProfiles).toContain("ssh://profile-2/");
    });

    it("profiles added while hidden start polling when visible again", async () => {
      // Start hidden
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.route).not.toHaveBeenCalled();

      // Become visible
      monitor.setVisible(true);
      await vi.advanceTimersByTimeAsync(0);

      expect(mockRouter.route).toHaveBeenCalledWith(
        "ssh://profile-1/",
        "system_health",
        { include_gpu: true },
      );
    });

    it("isVisible returns current visibility state", () => {
      expect(monitor.isVisible()).toBe(false);
      monitor.setVisible(true);
      expect(monitor.isVisible()).toBe(true);
      monitor.setVisible(false);
      expect(monitor.isVisible()).toBe(false);
    });

    it("setVisible(true) when already visible is a no-op", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.route).toHaveBeenCalledTimes(1);

      // Calling setVisible(true) again should not restart intervals
      monitor.setVisible(true);
      await vi.advanceTimersByTimeAsync(0);
      // No additional immediate poll
      expect(mockRouter.route).toHaveBeenCalledTimes(1);
    });
  });

  describe("error handling", () => {
    it("emits health:error when RPC fails", async () => {
      mockRouter = makeMockRouter(new Error("Connection lost"), true);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const errors: Array<{ profileId: string; error: string }> = [];
      monitor.on("health:error", (payload) => errors.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(errors).toHaveLength(1);
      expect(errors[0].profileId).toBe("profile-1");
      expect(errors[0].error).toBe("Connection lost");
    });

    it("returns cached health on RPC failure if available", async () => {
      const healthData = makeHealthResponse();
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      // Now make the router fail
      mockRouter.route.mockRejectedValue(new Error("Timeout"));

      // Advance to next poll
      await vi.advanceTimersByTimeAsync(1000);

      // Cached health should still be available
      const cached = monitor.getLastHealth("profile-1");
      expect(cached).toBeDefined();
      expect(cached!.os).toBe("Linux");
    });

    it("continues polling after an error", async () => {
      let callCount = 0;
      mockRouter.route.mockImplementation(() => {
        callCount++;
        if (callCount === 2) {
          return Promise.reject(new Error("Temporary failure"));
        }
        return Promise.resolve(makeHealthResponse());
      });

      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      // First poll succeeds
      await vi.advanceTimersByTimeAsync(0);
      expect(mockRouter.route).toHaveBeenCalledTimes(1);

      // Second poll fails
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.route).toHaveBeenCalledTimes(2);

      // Third poll should still happen (polling continues)
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.route).toHaveBeenCalledTimes(3);
    });

    it("handles non-Error rejection values", async () => {
      mockRouter = makeMockRouter("string error", true);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const errors: Array<{ profileId: string; error: string }> = [];
      monitor.on("health:error", (payload) => errors.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(errors).toHaveLength(1);
      expect(errors[0].error).toBe("string error");
    });
  });

  describe("cache behavior", () => {
    it("caches the last health result per profile", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      const cached = monitor.getLastHealth("profile-1");
      expect(cached).toBeDefined();
      expect(cached!.hostname).toBe("gpu-server-01");
    });

    it("returns undefined for profiles that have never been polled", () => {
      expect(monitor.getLastHealth("unknown-profile")).toBeUndefined();
    });

    it("updates cache on each successful poll", async () => {
      let pollCount = 0;
      mockRouter.route.mockImplementation(() => {
        pollCount++;
        return Promise.resolve(
          makeHealthResponse({ uptime: pollCount * 1000 }),
        );
      });

      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      await vi.advanceTimersByTimeAsync(0);
      expect(monitor.getLastHealth("profile-1")!.uptime).toBe(1000);

      await vi.advanceTimersByTimeAsync(1000);
      expect(monitor.getLastHealth("profile-1")!.uptime).toBe(2000);
    });

    it("preserves cache after stopPolling", async () => {
      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      monitor.stopPolling("profile-1");

      // Cache should still be accessible
      const cached = monitor.getLastHealth("profile-1");
      expect(cached).toBeDefined();
      expect(cached!.hostname).toBe("gpu-server-01");
    });

    it("maintains separate caches per profile", async () => {
      let callIdx = 0;
      mockRouter.route.mockImplementation((repoId: string) => {
        callIdx++;
        if (repoId.includes("profile-1")) {
          return Promise.resolve(makeHealthResponse({ hostname: "server-1" }));
        }
        return Promise.resolve(makeHealthResponse({ hostname: "server-2" }));
      });

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      monitor.startPolling("profile-2");
      await vi.advanceTimersByTimeAsync(0);

      expect(monitor.getLastHealth("profile-1")!.hostname).toBe("server-1");
      expect(monitor.getLastHealth("profile-2")!.hostname).toBe("server-2");
    });

    it("refresh forces an immediate poll and returns the result", async () => {
      monitor.setVisible(true);

      const result = await monitor.refresh("profile-1");

      expect(result).toBeDefined();
      expect(result.os).toBe("Linux");
      expect(monitor.getLastHealth("profile-1")).toBeDefined();
    });
  });

  describe("health alerts", () => {
    it("emits disk warning when usage exceeds 90%", async () => {
      const healthData = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 5, usedPercent: 95 }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(alerts).toHaveLength(1);
      expect(alerts[0].alert.type).toBe("disk");
      expect(alerts[0].alert.severity).toBe("critical");
    });

    it("emits memory warning when usage exceeds 90%", async () => {
      const healthData = makeHealthResponse({
        memory: { total: 128_000, free: 5_000, cached: 2_000, usedPercent: 96 },
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(alerts.some((a) => a.alert.type === "memory")).toBe(true);
    });

    it("emits GPU temperature warning when temp exceeds 85C", async () => {
      const healthData = makeHealthResponse({
        gpus: [{
          index: 0,
          name: "NVIDIA A100",
          utilization: 99,
          memoryUsed: 38_000_000_000,
          memoryTotal: 40_000_000_000,
          temperature: 88,
          processes: [],
        }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(alerts.some((a) => a.alert.type === "gpu_temp")).toBe(true);
      expect(alerts.find((a) => a.alert.type === "gpu_temp")!.alert.severity).toBe("warning");
    });
  });

  describe("formatSummary", () => {
    it("formats a summary with GPU, disk, and memory", () => {
      const health = makeHealthResponse();
      const summary = monitor.formatSummary(health);

      expect(summary).toContain("A100");
      expect(summary).toContain("87% util");
      expect(summary).toContain("free");
      expect(summary).toContain("RAM");
    });

    it("formats summary without GPU when none present", () => {
      const health = makeHealthResponse({ gpus: undefined });
      const summary = monitor.formatSummary(health);

      expect(summary).not.toContain("util");
      expect(summary).toContain("free");
      expect(summary).toContain("RAM");
    });
  });

  describe("GPU data handling", () => {
    it("handles health response with no GPUs (nvidia-smi not installed)", async () => {
      const healthData = makeHealthResponse({ gpus: undefined });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const updates: Array<{ profileId: string; health: SystemHealth }> = [];
      monitor.on("ssh:health-update", (payload) => updates.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(updates).toHaveLength(1);
      expect(updates[0].health.gpus).toBeUndefined();
    });

    it("handles health response with null GPUs (driver not loaded)", async () => {
      const healthData = makeHealthResponse({ gpus: null as any });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const updates: Array<{ profileId: string; health: SystemHealth }> = [];
      monitor.on("ssh:health-update", (payload) => updates.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(updates).toHaveLength(1);
      expect(updates[0].health.gpus).toBeNull();
    });

    it("handles multiple GPUs in health response", async () => {
      const healthData = makeHealthResponse({
        gpus: [
          {
            index: 0,
            name: "NVIDIA A100",
            utilization: 87,
            memoryUsed: 30_000_000_000,
            memoryTotal: 40_000_000_000,
            temperature: 72,
            processes: [{ pid: 1234, name: "python", memUsed: 28_000_000_000 }],
          },
          {
            index: 1,
            name: "NVIDIA A100",
            utilization: 92,
            memoryUsed: 38_000_000_000,
            memoryTotal: 40_000_000_000,
            temperature: 75,
            processes: [{ pid: 5678, name: "train.py", memUsed: 36_000_000_000 }],
          },
          {
            index: 2,
            name: "NVIDIA A100",
            utilization: 15,
            memoryUsed: 2_000_000_000,
            memoryTotal: 40_000_000_000,
            temperature: 45,
            processes: [],
          },
          {
            index: 3,
            name: "NVIDIA A100",
            utilization: 0,
            memoryUsed: 500_000_000,
            memoryTotal: 40_000_000_000,
            temperature: 38,
            processes: [],
          },
        ],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const updates: Array<{ profileId: string; health: SystemHealth }> = [];
      monitor.on("ssh:health-update", (payload) => updates.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(updates).toHaveLength(1);
      const gpus = updates[0].health.gpus!;
      expect(gpus).toHaveLength(4);
      expect(gpus[0].utilization).toBe(87);
      expect(gpus[1].utilization).toBe(92);
      expect(gpus[2].processes).toHaveLength(0);
      expect(gpus[3].utilization).toBe(0);
    });

    it("handles GPU with no running processes", async () => {
      const healthData = makeHealthResponse({
        gpus: [
          {
            index: 0,
            name: "NVIDIA RTX 4090",
            utilization: 0,
            memoryUsed: 256_000_000,
            memoryTotal: 24_000_000_000,
            temperature: 35,
            processes: [],
          },
        ],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const updates: Array<{ profileId: string; health: SystemHealth }> = [];
      monitor.on("ssh:health-update", (payload) => updates.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      const gpu = updates[0].health.gpus![0];
      expect(gpu.processes).toEqual([]);
      expect(gpu.utilization).toBe(0);
    });

    it("does not emit GPU temperature alert when temp is below threshold", async () => {
      const healthData = makeHealthResponse({
        gpus: [{
          index: 0,
          name: "NVIDIA A100",
          utilization: 99,
          memoryUsed: 38_000_000_000,
          memoryTotal: 40_000_000_000,
          temperature: 70, // Below 85C threshold
          processes: [],
        }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(alerts.filter((a) => a.alert.type === "gpu_temp")).toHaveLength(0);
    });

    it("emits critical GPU temperature alert when temp exceeds 90C", async () => {
      const healthData = makeHealthResponse({
        gpus: [{
          index: 0,
          name: "NVIDIA A100",
          utilization: 100,
          memoryUsed: 40_000_000_000,
          memoryTotal: 40_000_000_000,
          temperature: 92,
          processes: [],
        }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      const gpuAlert = alerts.find((a) => a.alert.type === "gpu_temp");
      expect(gpuAlert).toBeDefined();
      expect(gpuAlert!.alert.severity).toBe("critical");
      expect(gpuAlert!.alert.value).toBe(92);
    });

    it("emits alerts for multiple overheating GPUs", async () => {
      const healthData = makeHealthResponse({
        gpus: [
          { index: 0, name: "GPU 0", utilization: 100, memoryUsed: 0, memoryTotal: 0, temperature: 88, processes: [] },
          { index: 1, name: "GPU 1", utilization: 100, memoryUsed: 0, memoryTotal: 0, temperature: 91, processes: [] },
        ],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      const gpuAlerts = alerts.filter((a) => a.alert.type === "gpu_temp");
      expect(gpuAlerts).toHaveLength(2);
      expect(gpuAlerts[0].alert.severity).toBe("warning"); // 88C
      expect(gpuAlerts[1].alert.severity).toBe("critical"); // 91C
    });

    it("formatSummary shows average utilization across multiple GPUs", () => {
      const health = makeHealthResponse({
        gpus: [
          { index: 0, name: "NVIDIA A100", utilization: 80, memoryUsed: 0, memoryTotal: 0, temperature: 70, processes: [] },
          { index: 1, name: "NVIDIA A100", utilization: 60, memoryUsed: 0, memoryTotal: 0, temperature: 70, processes: [] },
          { index: 2, name: "NVIDIA A100", utilization: 100, memoryUsed: 0, memoryTotal: 0, temperature: 70, processes: [] },
          { index: 3, name: "NVIDIA A100", utilization: 40, memoryUsed: 0, memoryTotal: 0, temperature: 70, processes: [] },
        ],
      });
      const summary = monitor.formatSummary(health);

      // Average: (80+60+100+40)/4 = 70
      expect(summary).toContain("4x A100");
      expect(summary).toContain("70% util");
    });

    it("formatSummary strips NVIDIA prefix from GPU name", () => {
      const health = makeHealthResponse({
        gpus: [
          { index: 0, name: "NVIDIA GeForce RTX 4090", utilization: 50, memoryUsed: 0, memoryTotal: 0, temperature: 60, processes: [] },
        ],
      });
      const summary = monitor.formatSummary(health);

      expect(summary).toContain("1x GeForce RTX 4090");
      expect(summary).not.toContain("NVIDIA");
    });
  });

  describe("alert deduplication", () => {
    it("does not re-emit the same alert on consecutive polls", async () => {
      const healthData = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 5, usedPercent: 95 }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      // First poll — alert should fire
      await vi.advanceTimersByTimeAsync(0);
      expect(alerts).toHaveLength(1);
      expect(alerts[0].alert.type).toBe("disk");

      // Second poll — same condition, alert should NOT fire again
      await vi.advanceTimersByTimeAsync(1000);
      expect(alerts).toHaveLength(1);

      // Third poll — still the same, no new alert
      await vi.advanceTimersByTimeAsync(1000);
      expect(alerts).toHaveLength(1);
    });

    it("emits alert again after it clears and re-triggers", async () => {
      // Start with a disk alert
      const highDisk = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 5, usedPercent: 95 }],
      });
      let currentResponse = highDisk;
      const dynamicRouter = {
        route: vi.fn().mockImplementation(() => Promise.resolve({ ...currentResponse, collectedAt: Date.now() })),
        batchRpc: vi.fn(),
      } as any;

      monitor = new SystemHealthMonitor(dynamicRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");

      // First poll — alert fires
      await vi.advanceTimersByTimeAsync(0);
      expect(alerts).toHaveLength(1);

      // Condition clears
      currentResponse = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 50, usedPercent: 50 }],
      });
      await vi.advanceTimersByTimeAsync(1000);
      expect(alerts).toHaveLength(1); // No new alert

      // Condition re-triggers — alert should fire again
      currentResponse = highDisk;
      await vi.advanceTimersByTimeAsync(1000);
      expect(alerts).toHaveLength(2);
      expect(alerts[1].alert.type).toBe("disk");
    });

    it("emits alerts-snapshot with the full list of current alerts", async () => {
      const healthData = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 5, usedPercent: 95 }],
        memory: { total: 100, free: 5, cached: 0, usedPercent: 95 },
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const snapshots: Array<{ profileId: string; alerts: any[] }> = [];
      monitor.on("health:alerts-snapshot", (payload) => snapshots.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(snapshots).toHaveLength(1);
      expect(snapshots[0].alerts).toHaveLength(2);
      expect(snapshots[0].alerts.map((a: any) => a.type).sort()).toEqual(["disk", "memory"]);
    });

    it("clears active alert keys when stopPolling is called", async () => {
      const healthData = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 5, usedPercent: 95 }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);

      expect(monitor.getActiveAlertKeys("profile-1").size).toBeGreaterThan(0);

      monitor.stopPolling("profile-1");
      expect(monitor.getActiveAlertKeys("profile-1").size).toBe(0);
    });

    it("tracks alerts independently per profile", async () => {
      const healthData = makeHealthResponse({
        disk: [{ path: "/", total: 100, free: 5, usedPercent: 95 }],
      });
      mockRouter = makeMockRouter(healthData);
      monitor = new SystemHealthMonitor(mockRouter, makeMockSessionManager(), 1000);

      const alerts: Array<{ profileId: string; alert: any }> = [];
      monitor.on("health:alert", (payload) => alerts.push(payload));

      monitor.setVisible(true);
      monitor.startPolling("profile-1");
      await vi.advanceTimersByTimeAsync(0);
      expect(alerts).toHaveLength(1);

      // Start polling for a second profile — same condition should trigger
      // a new alert for the second profile
      monitor.startPolling("profile-2");
      await vi.advanceTimersByTimeAsync(0);
      expect(alerts).toHaveLength(2);
      expect(alerts[1].profileId).toBe("profile-2");
    });
  });
});
