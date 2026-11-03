/**
 * Unit tests for the SshLatencyMonitor class.
 *
 * Covers: periodic ping measurement, event emission, cache integration,
 * start/stop lifecycle, and error handling for failed pings.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { SshLatencyMonitor } from "../electron/ssh-latency-monitor";

// ── Test helpers ─────────────────────────────────────────────────

function makeMockRouter(latencyMs = 10) {
  const sendToRemoteAgent = vi.fn().mockImplementation(
    () => new Promise((resolve) => setTimeout(() => resolve({ pong: "1.0.0" }), latencyMs)),
  );
  return { sendToRemoteAgent };
}

function makeMockSessionManager(status: string = "connected") {
  return {
    getStatus: vi.fn().mockReturnValue(status),
  };
}

function makeMockCache() {
  return {
    updateLatency: vi.fn(),
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("SshLatencyMonitor", () => {
  let monitor: SshLatencyMonitor;
  let mockRouter: ReturnType<typeof makeMockRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;
  let mockCache: ReturnType<typeof makeMockCache>;

  beforeEach(() => {
    vi.useFakeTimers();
    mockRouter = makeMockRouter(5);
    mockSessionManager = makeMockSessionManager("connected");
    mockCache = makeMockCache();
    monitor = new SshLatencyMonitor(
      mockRouter as any,
      mockSessionManager as any,
      mockCache as any,
      1000, // 1s interval for tests
    );
  });

  afterEach(() => {
    monitor.stopAll();
    vi.useRealTimers();
  });

  describe("startMonitoring", () => {
    it("performs an immediate latency measurement on start", async () => {
      monitor.startMonitoring("profile-1");

      // Flush the immediate async measurement
      await vi.advanceTimersByTimeAsync(10);

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "ping", {});
    });

    it("does not start duplicate timers for the same profile", () => {
      monitor.startMonitoring("profile-1");
      monitor.startMonitoring("profile-1");

      expect(monitor.getMonitoredProfiles()).toHaveLength(1);
    });

    it("can monitor multiple profiles simultaneously", () => {
      monitor.startMonitoring("profile-1");
      monitor.startMonitoring("profile-2");

      expect(monitor.getMonitoredProfiles()).toHaveLength(2);
      expect(monitor.getMonitoredProfiles()).toContain("profile-1");
      expect(monitor.getMonitoredProfiles()).toContain("profile-2");
    });
  });

  describe("periodic measurement", () => {
    it("measures latency at the configured interval", async () => {
      monitor.startMonitoring("profile-1");

      // Immediate measurement
      await vi.advanceTimersByTimeAsync(10);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(1);

      // After one interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(2);

      // After another interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(3);
    });
  });

  describe("event emission", () => {
    it("emits ssh:latency-update with profileId and latencyMs", async () => {
      const updates: Array<{ profileId: string; latencyMs: number }> = [];
      monitor.on("ssh:latency-update", (payload) => updates.push(payload));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(updates).toHaveLength(1);
      expect(updates[0].profileId).toBe("profile-1");
      expect(updates[0].latencyMs).toBeGreaterThanOrEqual(0);
    });
  });

  describe("cache integration", () => {
    it("calls cache.updateLatency with the measured value", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(mockCache.updateLatency).toHaveBeenCalledWith("profile-1", expect.any(Number));
    });

    it("works without a cache (null cache)", async () => {
      const noCacheMonitor = new SshLatencyMonitor(
        mockRouter as any,
        mockSessionManager as any,
        null,
        1000,
      );

      const updates: Array<{ profileId: string; latencyMs: number }> = [];
      noCacheMonitor.on("ssh:latency-update", (payload) => updates.push(payload));

      noCacheMonitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(updates).toHaveLength(1);
      noCacheMonitor.stopAll();
    });
  });

  describe("connection status check", () => {
    it("skips measurement when connection is not in connected state", async () => {
      mockSessionManager.getStatus.mockReturnValue("reconnecting");

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
    });

    it("skips measurement when connection is disconnected", async () => {
      mockSessionManager.getStatus.mockReturnValue("disconnected");

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
    });
  });

  describe("error handling", () => {
    it("does not emit latency-update when ping fails", async () => {
      mockRouter.sendToRemoteAgent.mockRejectedValue(new Error("RPC timeout"));

      const updates: Array<{ profileId: string; latencyMs: number }> = [];
      monitor.on("ssh:latency-update", (payload) => updates.push(payload));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(updates).toHaveLength(0);
    });

    it("does not crash when ping throws", async () => {
      mockRouter.sendToRemoteAgent.mockRejectedValue(new Error("Channel closed"));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      // Should not throw — verify by checking the timer is still active
      expect(monitor.getMonitoredProfiles()).toContain("profile-1");
    });

    it("continues polling after a failed measurement", async () => {
      // First call fails, second succeeds
      mockRouter.sendToRemoteAgent
        .mockRejectedValueOnce(new Error("timeout"))
        .mockResolvedValue({ pong: "1.0.0" });

      const updates: Array<{ profileId: string; latencyMs: number }> = [];
      monitor.on("ssh:latency-update", (payload) => updates.push(payload));

      monitor.startMonitoring("profile-1");

      // First measurement (fails)
      await vi.advanceTimersByTimeAsync(10);
      expect(updates).toHaveLength(0);

      // Second measurement (succeeds after interval)
      await vi.advanceTimersByTimeAsync(1000);
      expect(updates).toHaveLength(1);
    });
  });

  describe("stopMonitoring", () => {
    it("stops the timer for a specific profile", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      monitor.stopMonitoring("profile-1");

      // Advance past the next interval — should not trigger another call
      const callCount = mockRouter.sendToRemoteAgent.mock.calls.length;
      await vi.advanceTimersByTimeAsync(2000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(callCount);
    });

    it("removes the profile from monitored list", () => {
      monitor.startMonitoring("profile-1");
      expect(monitor.getMonitoredProfiles()).toContain("profile-1");

      monitor.stopMonitoring("profile-1");
      expect(monitor.getMonitoredProfiles()).not.toContain("profile-1");
    });

    it("is safe to call for a profile that is not being monitored", () => {
      expect(() => monitor.stopMonitoring("nonexistent")).not.toThrow();
    });
  });

  describe("stopAll", () => {
    it("stops all active monitors", async () => {
      monitor.startMonitoring("profile-1");
      monitor.startMonitoring("profile-2");
      await vi.advanceTimersByTimeAsync(10);

      monitor.stopAll();

      expect(monitor.getMonitoredProfiles()).toHaveLength(0);

      // Advance time — no more calls should happen
      const callCount = mockRouter.sendToRemoteAgent.mock.calls.length;
      await vi.advanceTimersByTimeAsync(5000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(callCount);
    });
  });

  describe("getLastLatency", () => {
    it("returns the last measured latency for a profile", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      const latency = monitor.getLastLatency("profile-1");
      expect(latency).toBeGreaterThanOrEqual(0);
    });

    it("returns undefined for a profile with no measurements", () => {
      expect(monitor.getLastLatency("unknown")).toBeUndefined();
    });
  });

  describe("high-latency mode", () => {
    it("activates immediately when latency exceeds 200ms", async () => {
      // Recreate monitor with high-latency router
      monitor.stopAll();
      mockRouter = makeMockRouter(250);
      monitor = new SshLatencyMonitor(
        mockRouter as any,
        mockSessionManager as any,
        mockCache as any,
        1000,
      );

      const events: Array<{ profileId: string; enabled: boolean }> = [];
      monitor.on("ssh:high-latency-mode", (payload) => events.push(payload));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(300);

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ profileId: "profile-1", enabled: true });
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);
    });

    it("does not activate when latency is at or below 200ms", async () => {
      // Default mock has 5ms latency
      const events: Array<{ profileId: string; enabled: boolean }> = [];
      monitor.on("ssh:high-latency-mode", (payload) => events.push(payload));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10);

      expect(events).toHaveLength(0);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(false);
    });

    it("deactivates after 3 consecutive low-latency measurements", async () => {
      monitor.stopAll();
      // Start with high latency
      mockRouter = makeMockRouter(250);
      monitor = new SshLatencyMonitor(
        mockRouter as any,
        mockSessionManager as any,
        mockCache as any,
        1000,
      );

      const events: Array<{ profileId: string; enabled: boolean }> = [];
      monitor.on("ssh:high-latency-mode", (payload) => events.push(payload));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(300);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);

      // Switch to low latency
      mockRouter.sendToRemoteAgent.mockImplementation(
        () => new Promise((resolve) => setTimeout(() => resolve({ pong: "1.0.0" }), 30)),
      );

      // 1st low measurement
      await vi.advanceTimersByTimeAsync(1050);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);

      // 2nd low measurement
      await vi.advanceTimersByTimeAsync(1050);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);

      // 3rd low measurement — deactivates
      await vi.advanceTimersByTimeAsync(1050);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(false);
      expect(events).toHaveLength(2);
      expect(events[1]).toEqual({ profileId: "profile-1", enabled: false });
    });

    it("resets the low-latency streak on a high-latency spike", async () => {
      monitor.stopAll();
      mockRouter = makeMockRouter(250);
      monitor = new SshLatencyMonitor(
        mockRouter as any,
        mockSessionManager as any,
        mockCache as any,
        1000,
      );

      const events: Array<{ profileId: string; enabled: boolean }> = [];
      monitor.on("ssh:high-latency-mode", (payload) => events.push(payload));

      // Activate
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(300);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);

      // 2 low measurements
      mockRouter.sendToRemoteAgent.mockImplementation(
        () => new Promise((resolve) => setTimeout(() => resolve({ pong: "1.0.0" }), 30)),
      );
      await vi.advanceTimersByTimeAsync(1050);
      await vi.advanceTimersByTimeAsync(1050);

      // Spike back to high latency (resets streak)
      mockRouter.sendToRemoteAgent.mockImplementation(
        () => new Promise((resolve) => setTimeout(() => resolve({ pong: "1.0.0" }), 250)),
      );
      await vi.advanceTimersByTimeAsync(1050);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);

      // Need 3 fresh consecutive low measurements
      mockRouter.sendToRemoteAgent.mockImplementation(
        () => new Promise((resolve) => setTimeout(() => resolve({ pong: "1.0.0" }), 30)),
      );
      await vi.advanceTimersByTimeAsync(1050);
      await vi.advanceTimersByTimeAsync(1050);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);
      await vi.advanceTimersByTimeAsync(1050);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(false);
    });

    it("does not emit duplicate activation events", async () => {
      monitor.stopAll();
      mockRouter = makeMockRouter(250);
      monitor = new SshLatencyMonitor(
        mockRouter as any,
        mockSessionManager as any,
        mockCache as any,
        1000,
      );

      const events: Array<{ profileId: string; enabled: boolean }> = [];
      monitor.on("ssh:high-latency-mode", (payload) => events.push(payload));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(300);
      await vi.advanceTimersByTimeAsync(1050);
      await vi.advanceTimersByTimeAsync(1050);

      // Only one activation event despite multiple high-latency measurements
      expect(events).toHaveLength(1);
    });

    it("cleans up high-latency state on stopMonitoring", async () => {
      monitor.stopAll();
      mockRouter = makeMockRouter(250);
      monitor = new SshLatencyMonitor(
        mockRouter as any,
        mockSessionManager as any,
        mockCache as any,
        1000,
      );

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(300);
      expect(monitor.isHighLatencyMode("profile-1")).toBe(true);

      monitor.stopMonitoring("profile-1");
      expect(monitor.isHighLatencyMode("profile-1")).toBe(false);
    });

    it("returns false for isHighLatencyMode on unknown profiles", () => {
      expect(monitor.isHighLatencyMode("unknown")).toBe(false);
    });
  });
});
