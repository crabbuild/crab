/**
 * Unit tests for the ConnectionQualityMonitor class.
 *
 * Covers: quality level computation, event emission on transitions,
 * 10-second measurement intervals, packet loss detection on timeout,
 * bandwidth estimation, start/stop lifecycle.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { ConnectionQualityMonitor } from "../electron/ssh-connection-quality";

// ── Test helpers ─────────────────────────────────────────────────

function makeMockLatencyMonitor() {
  return {
    getLastLatency: vi.fn().mockReturnValue(50),
    startMonitoring: vi.fn(),
    stopMonitoring: vi.fn(),
  };
}

function makeMockRouter(responseDelayMs = 10, responseSize = 4096) {
  const sendToRemoteAgent = vi.fn().mockImplementation(
    () =>
      new Promise((resolve) =>
        setTimeout(() => resolve({ responseSize }), responseDelayMs),
      ),
  );
  return { sendToRemoteAgent };
}

function makeMockSessionManager(status: string = "connected") {
  return {
    getStatus: vi.fn().mockReturnValue(status),
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("ConnectionQualityMonitor", () => {
  let monitor: ConnectionQualityMonitor;
  let mockLatencyMonitor: ReturnType<typeof makeMockLatencyMonitor>;
  let mockRouter: ReturnType<typeof makeMockRouter>;
  let mockSessionManager: ReturnType<typeof makeMockSessionManager>;

  beforeEach(() => {
    vi.useFakeTimers();
    mockLatencyMonitor = makeMockLatencyMonitor();
    mockRouter = makeMockRouter(10);
    mockSessionManager = makeMockSessionManager("connected");
    monitor = new ConnectionQualityMonitor(
      mockLatencyMonitor as any,
      mockRouter as any,
      mockSessionManager as any,
      1000, // 1s interval for tests
    );
  });

  afterEach(() => {
    monitor.stopAll();
    vi.useRealTimers();
  });

  describe("startMonitoring", () => {
    it("performs an immediate measurement on start", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledWith(
        "profile-1",
        "bandwidth_probe",
        { payloadSize: 4096 },
      );
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
    });
  });

  describe("periodic measurement", () => {
    it("measures at the configured interval", async () => {
      monitor.startMonitoring("profile-1");

      // Immediate measurement
      await vi.advanceTimersByTimeAsync(50);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(1);

      // After one interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(2);

      // After another interval
      await vi.advanceTimersByTimeAsync(1000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(3);
    });
  });

  describe("quality level computation", () => {
    it("reports green when latency is low and no packet loss", async () => {
      // Default mock: 10ms response, no loss
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.level).toBe("green");
      expect(quality!.packetLossPercent).toBe(0);
    });

    it("reports yellow when latency is between 100-200ms", async () => {
      monitor.stopAll();
      mockRouter = makeMockRouter(150);
      monitor = new ConnectionQualityMonitor(
        mockLatencyMonitor as any,
        mockRouter as any,
        mockSessionManager as any,
        1000,
      );

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(200);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.level).toBe("yellow");
    });

    it("reports red when latency exceeds 200ms", async () => {
      monitor.stopAll();
      mockRouter = makeMockRouter(250);
      monitor = new ConnectionQualityMonitor(
        mockLatencyMonitor as any,
        mockRouter as any,
        mockSessionManager as any,
        1000,
      );

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(300);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.level).toBe("red");
    });

    it("reports red when packet loss exceeds 15%", async () => {
      monitor.stopAll();
      // All probes reject immediately to simulate packet loss
      const sendToRemoteAgent = vi.fn().mockRejectedValue(new Error("timeout"));
      mockRouter = { sendToRemoteAgent } as any;
      monitor = new ConnectionQualityMonitor(
        mockLatencyMonitor as any,
        mockRouter as any,
        mockSessionManager as any,
        15_000, // long interval so only one measurement fires
      );

      monitor.startMonitoring("profile-1");

      // Advance past the 10s probe timeout
      await vi.advanceTimersByTimeAsync(10_100);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.level).toBe("red");
      expect(quality!.packetLossPercent).toBe(100);
    });
  });

  describe("packet loss detection", () => {
    it("treats missing response within timeout as 100% packet loss", async () => {
      monitor.stopAll();
      // Mock that never resolves within timeout
      const sendToRemoteAgent = vi.fn().mockImplementation(
        () => new Promise((_resolve, reject) =>
          setTimeout(() => reject(new Error("timeout")), 11_000),
        ),
      );
      mockRouter = { sendToRemoteAgent } as any;
      monitor = new ConnectionQualityMonitor(
        mockLatencyMonitor as any,
        mockRouter as any,
        mockSessionManager as any,
        15_000, // interval longer than timeout
      );

      monitor.startMonitoring("profile-1");

      // Advance past the 10s probe timeout
      await vi.advanceTimersByTimeAsync(10_100);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.packetLossPercent).toBe(100);
      expect(quality!.level).toBe("red");
    });
  });

  describe("quality-changed event", () => {
    it("emits quality-changed when level transitions", async () => {
      const events: Array<{
        profileId: string;
        previous: string;
        current: string;
      }> = [];
      monitor.on("quality-changed", (event) => events.push(event));

      // Start with green (low latency)
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);
      expect(monitor.getQuality("profile-1")!.level).toBe("green");

      // Switch to high latency (red)
      mockRouter.sendToRemoteAgent.mockImplementation(
        () =>
          new Promise((resolve) =>
            setTimeout(() => resolve({ responseSize: 4096 }), 250),
          ),
      );

      await vi.advanceTimersByTimeAsync(1250);

      expect(events.length).toBeGreaterThanOrEqual(1);
      const lastEvent = events[events.length - 1];
      expect(lastEvent.profileId).toBe("profile-1");
      expect(lastEvent.previous).toBe("green");
    });

    it("does not emit when level stays the same", async () => {
      const events: Array<any> = [];
      monitor.on("quality-changed", (event) => events.push(event));

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      // Second measurement, still green
      await vi.advanceTimersByTimeAsync(1000);

      // No transition events (first measurement doesn't emit since there's no previous)
      expect(events).toHaveLength(0);
    });

    it("includes quality metrics in the event", async () => {
      const events: Array<any> = [];
      monitor.on("quality-changed", (event) => events.push(event));

      // Start green
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      // Transition to red
      mockRouter.sendToRemoteAgent.mockImplementation(
        () =>
          new Promise((resolve) =>
            setTimeout(() => resolve({ responseSize: 4096 }), 250),
          ),
      );
      await vi.advanceTimersByTimeAsync(1250);

      expect(events.length).toBeGreaterThanOrEqual(1);
      const event = events[events.length - 1];
      expect(event.quality).toBeDefined();
      expect(event.quality.latencyMs).toBeGreaterThan(0);
      expect(typeof event.quality.bandwidthMbps).toBe("number");
      expect(typeof event.quality.packetLossPercent).toBe("number");
      expect(event.quality.measuredAt).toBeGreaterThan(0);
    });
  });

  describe("bandwidth estimation", () => {
    it("estimates bandwidth from probe round-trip time", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.bandwidthMbps).toBeGreaterThan(0);
    });

    it("reports zero bandwidth on packet loss", async () => {
      monitor.stopAll();
      const sendToRemoteAgent = vi.fn().mockRejectedValue(new Error("timeout"));
      mockRouter = { sendToRemoteAgent } as any;
      monitor = new ConnectionQualityMonitor(
        mockLatencyMonitor as any,
        mockRouter as any,
        mockSessionManager as any,
        1000,
      );

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(10_100);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.bandwidthMbps).toBe(0);
    });
  });

  describe("connection status check", () => {
    it("skips measurement when not connected", async () => {
      mockSessionManager.getStatus.mockReturnValue("disconnected");

      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      expect(mockRouter.sendToRemoteAgent).not.toHaveBeenCalled();
      expect(monitor.getQuality("profile-1")).toBeNull();
    });
  });

  describe("stopMonitoring", () => {
    it("stops the timer and clears state for a profile", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      monitor.stopMonitoring("profile-1");

      expect(monitor.getMonitoredProfiles()).not.toContain("profile-1");
      expect(monitor.getQuality("profile-1")).toBeNull();

      // No more measurements
      const callCount = mockRouter.sendToRemoteAgent.mock.calls.length;
      await vi.advanceTimersByTimeAsync(5000);
      expect(mockRouter.sendToRemoteAgent).toHaveBeenCalledTimes(callCount);
    });

    it("is safe to call for a profile not being monitored", () => {
      expect(() => monitor.stopMonitoring("nonexistent")).not.toThrow();
    });
  });

  describe("stopAll", () => {
    it("stops all active monitors", async () => {
      monitor.startMonitoring("profile-1");
      monitor.startMonitoring("profile-2");
      await vi.advanceTimersByTimeAsync(50);

      monitor.stopAll();

      expect(monitor.getMonitoredProfiles()).toHaveLength(0);
      expect(monitor.getQuality("profile-1")).toBeNull();
      expect(monitor.getQuality("profile-2")).toBeNull();
    });
  });

  describe("getQuality", () => {
    it("returns null for a profile with no measurements", () => {
      expect(monitor.getQuality("unknown")).toBeNull();
    });

    it("returns the current quality after measurement", async () => {
      monitor.startMonitoring("profile-1");
      await vi.advanceTimersByTimeAsync(50);

      const quality = monitor.getQuality("profile-1");
      expect(quality).not.toBeNull();
      expect(quality!.level).toBeDefined();
      expect(quality!.latencyMs).toBeGreaterThanOrEqual(0);
      expect(quality!.measuredAt).toBeGreaterThan(0);
    });
  });
});
