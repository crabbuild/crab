/**
 * Unit tests for the BandwidthThrottler class.
 *
 * Covers: token bucket algorithm, consumeTokens blocking, setLimit,
 * getCurrentThroughput sliding window, unlimited mode, adaptive throttling
 * integration point, and destroy cleanup.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { BandwidthThrottler } from "../electron/ssh-bandwidth-throttler";
import { EventEmitter } from "node:events";

// ── Mock helpers ─────────────────────────────────────────────────

function makeMockLatencyMonitor() {
  const emitter = new EventEmitter();
  return Object.assign(emitter, {
    startMonitoring: vi.fn(),
    stopMonitoring: vi.fn(),
    stopAll: vi.fn(),
    getLastLatency: vi.fn().mockReturnValue(50),
    isHighLatencyMode: vi.fn().mockReturnValue(false),
    getMonitoredProfiles: vi.fn().mockReturnValue([]),
  });
}

// ── Tests ────────────────────────────────────────────────────────

describe("BandwidthThrottler", () => {
  let throttler: BandwidthThrottler;
  let mockLatencyMonitor: ReturnType<typeof makeMockLatencyMonitor>;

  beforeEach(() => {
    vi.useFakeTimers();
    mockLatencyMonitor = makeMockLatencyMonitor();
  });

  afterEach(() => {
    throttler?.destroy();
    vi.useRealTimers();
  });

  describe("unlimited mode (limit = 0)", () => {
    beforeEach(() => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 0,
        adaptiveEnabled: false,
        adaptiveFloor: 10 * 1024,
      });
    });

    it("resolves consumeTokens immediately when limit is 0", async () => {
      const start = Date.now();
      await throttler.consumeTokens(1_000_000);
      expect(Date.now() - start).toBe(0);
    });

    it("reports limit as 0", () => {
      expect(throttler.getLimit()).toBe(0);
    });

    it("reports effective limit as 0", () => {
      expect(throttler.getEffectiveLimit()).toBe(0);
    });
  });

  describe("token bucket with configured limit", () => {
    beforeEach(() => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 10_000, // 10 KB/s
        adaptiveEnabled: false,
        adaptiveFloor: 10 * 1024,
      });
    });

    it("allows immediate consumption when tokens are available", async () => {
      // After construction, refill timer hasn't fired yet but bucket starts empty.
      // Advance to trigger a refill first.
      await vi.advanceTimersByTimeAsync(100);

      // Now tokens should be available (10000 * 0.1 = 1000 bytes per 100ms interval)
      const resolved = vi.fn();
      throttler.consumeTokens(500).then(resolved);
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).toHaveBeenCalled();
    });

    it("blocks consumption when tokens are insufficient", async () => {
      // Advance to get some tokens
      await vi.advanceTimersByTimeAsync(100);

      // Try to consume more than available (bucket capacity is 2 * 1000 = 2000)
      const resolved = vi.fn();
      throttler.consumeTokens(5000).then(resolved);
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).not.toHaveBeenCalled();

      // After enough refills, it should resolve
      await vi.advanceTimersByTimeAsync(500);
      expect(resolved).toHaveBeenCalled();
    });

    it("drains pending queue in order", async () => {
      const order: number[] = [];

      // Consume all available tokens first
      await vi.advanceTimersByTimeAsync(100);

      // Queue multiple consumers that exceed available tokens
      throttler.consumeTokens(1500).then(() => order.push(1));
      throttler.consumeTokens(1500).then(() => order.push(2));

      // Advance time to allow refills
      await vi.advanceTimersByTimeAsync(500);

      expect(order).toEqual([1, 2]);
    });

    it("refills tokens at 100ms intervals", async () => {
      // At 10000 bytes/sec, each 100ms refill adds 1000 tokens
      // After 200ms we should have ~2000 tokens available
      await vi.advanceTimersByTimeAsync(200);

      const resolved = vi.fn();
      throttler.consumeTokens(2000).then(resolved);
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).toHaveBeenCalled();
    });
  });

  describe("setLimit", () => {
    beforeEach(() => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 10_000,
        adaptiveEnabled: false,
        adaptiveFloor: 10 * 1024,
      });
    });

    it("updates the limit", () => {
      throttler.setLimit(50_000);
      expect(throttler.getLimit()).toBe(50_000);
      expect(throttler.getEffectiveLimit()).toBe(50_000);
    });

    it("setting limit to 0 makes throttling unlimited", async () => {
      throttler.setLimit(0);
      expect(throttler.getLimit()).toBe(0);

      // Should resolve immediately
      const start = Date.now();
      await throttler.consumeTokens(1_000_000);
      expect(Date.now() - start).toBe(0);
    });

    it("flushes pending consumers when limit becomes unlimited", async () => {
      // Queue a large consumption that will block
      await vi.advanceTimersByTimeAsync(100);
      const resolved = vi.fn();
      throttler.consumeTokens(100_000).then(resolved);
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).not.toHaveBeenCalled();

      // Set limit to 0 (unlimited) — should flush pending
      throttler.setLimit(0);
      await vi.advanceTimersByTimeAsync(100);
      expect(resolved).toHaveBeenCalled();
    });

    it("applies new limit within 2 seconds", async () => {
      // Start with 10 KB/s
      throttler.setLimit(100_000); // 100 KB/s

      // After 2 seconds, the new rate should be fully in effect
      await vi.advanceTimersByTimeAsync(2000);

      // At 100 KB/s, 100ms refill = 10000 tokens per interval
      // Bucket capacity = 20000
      const resolved = vi.fn();
      throttler.consumeTokens(15_000).then(resolved);
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).toHaveBeenCalled();
    });

    it("rejects negative values by clamping to 0", () => {
      throttler.setLimit(-100);
      expect(throttler.getLimit()).toBe(0);
    });
  });

  describe("getCurrentThroughput", () => {
    beforeEach(() => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 0, // unlimited for throughput measurement tests
        adaptiveEnabled: false,
        adaptiveFloor: 10 * 1024,
      });
    });

    it("returns 0 when no data has been consumed", () => {
      expect(throttler.getCurrentThroughput()).toBe(0);
    });

    it("tracks throughput from consumed tokens", async () => {
      await throttler.consumeTokens(5000);
      await vi.advanceTimersByTimeAsync(1000);
      await throttler.consumeTokens(5000);

      const throughput = throttler.getCurrentThroughput();
      expect(throughput).toBeGreaterThan(0);
    });

    it("uses a sliding window that expires old samples", async () => {
      await throttler.consumeTokens(10_000);

      // Advance past the 5-second window
      await vi.advanceTimersByTimeAsync(6000);

      // Old sample should be pruned
      expect(throttler.getCurrentThroughput()).toBe(0);
    });

    it("measures aggregate throughput across multiple consumers", async () => {
      await throttler.consumeTokens(1000);
      await vi.advanceTimersByTimeAsync(100);
      await throttler.consumeTokens(1000);
      await vi.advanceTimersByTimeAsync(100);
      await throttler.consumeTokens(1000);

      const throughput = throttler.getCurrentThroughput();
      // 3000 bytes over ~200ms, but window uses at least 1s denominator
      expect(throughput).toBeGreaterThan(0);
      expect(throughput).toBeLessThanOrEqual(30_000); // reasonable upper bound
    });
  });

  describe("adaptive throttling integration", () => {
    beforeEach(() => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 100_000, // 100 KB/s
        adaptiveEnabled: true,
        adaptiveFloor: 10 * 1024,
      });
    });

    it("emits latency-update event on onLatencyUpdate", () => {
      const events: Array<{ profileId: string; latencyMs: number }> = [];
      throttler.on("latency-update", (payload) => events.push(payload));

      throttler.onLatencyUpdate("profile-1", 250);

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ profileId: "profile-1", latencyMs: 250 });
    });

    it("does not emit when adaptive is disabled", () => {
      throttler.setAdaptiveEnabled(false);

      const events: Array<{ profileId: string; latencyMs: number }> = [];
      throttler.on("latency-update", (payload) => events.push(payload));

      throttler.onLatencyUpdate("profile-1", 250);
      expect(events).toHaveLength(0);
    });

    it("does not emit when limit is 0 (unlimited)", () => {
      throttler.setLimit(0);

      const events: Array<{ profileId: string; latencyMs: number }> = [];
      throttler.on("latency-update", (payload) => events.push(payload));

      throttler.onLatencyUpdate("profile-1", 250);
      expect(events).toHaveLength(0);
    });

    it("activateAdaptiveReduction reduces effective limit to 50%", () => {
      throttler.activateAdaptiveReduction();

      expect(throttler.getEffectiveLimit()).toBe(50_000);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);
    });

    it("adaptive reduction respects the floor", () => {
      throttler.setLimit(15_000); // 50% = 7500, below floor of 10240
      throttler.activateAdaptiveReduction();

      expect(throttler.getEffectiveLimit()).toBe(10 * 1024);
    });

    it("deactivateAdaptiveReduction restores user limit", () => {
      throttler.activateAdaptiveReduction();
      expect(throttler.getEffectiveLimit()).toBe(50_000);

      throttler.deactivateAdaptiveReduction();
      expect(throttler.getEffectiveLimit()).toBe(100_000);
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
    });

    it("setAdaptiveEnabled(false) deactivates reduction", () => {
      throttler.activateAdaptiveReduction();
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      throttler.setAdaptiveEnabled(false);
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
      expect(throttler.getEffectiveLimit()).toBe(100_000);
    });
  });

  describe("adaptive throttling state machine (onLatencyUpdate)", () => {
    beforeEach(() => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 100_000, // 100 KB/s
        adaptiveEnabled: true,
        adaptiveFloor: 10 * 1024,
      });
    });

    it("activates reduction when latency exceeds 200ms", () => {
      throttler.onLatencyUpdate("profile-1", 201);

      expect(throttler.isAdaptiveReductionActive()).toBe(true);
      expect(throttler.getEffectiveLimit()).toBe(50_000);
    });

    it("does not activate reduction at exactly 200ms", () => {
      throttler.onLatencyUpdate("profile-1", 200);

      expect(throttler.isAdaptiveReductionActive()).toBe(false);
      expect(throttler.getEffectiveLimit()).toBe(100_000);
    });

    it("restores bandwidth after 10 consecutive readings below 150ms", () => {
      // First, activate reduction
      throttler.onLatencyUpdate("profile-1", 250);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      // Send 9 low-latency readings — not enough yet
      for (let i = 0; i < 9; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      // 10th reading triggers recovery
      throttler.onLatencyUpdate("profile-1", 100);
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
      expect(throttler.getEffectiveLimit()).toBe(100_000);
    });

    it("does not restore if a reading >= 150ms interrupts the streak", () => {
      // Activate reduction
      throttler.onLatencyUpdate("profile-1", 250);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      // Send 5 low-latency readings
      for (let i = 0; i < 5; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }

      // Interrupt with a reading in the dead zone (150ms)
      throttler.onLatencyUpdate("profile-1", 150);

      // Send 9 more low-latency readings — counter was reset, so not enough
      for (let i = 0; i < 9; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      // 10th consecutive low reading after reset triggers recovery
      throttler.onLatencyUpdate("profile-1", 100);
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
    });

    it("resets recovery counter when latency exceeds 200ms again", () => {
      // Activate reduction
      throttler.onLatencyUpdate("profile-1", 250);

      // Send 8 low-latency readings
      for (let i = 0; i < 8; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }

      // High latency spike resets counter
      throttler.onLatencyUpdate("profile-1", 300);

      // Need full 10 consecutive readings again
      for (let i = 0; i < 9; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      throttler.onLatencyUpdate("profile-1", 100);
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
    });

    it("enforces minimum floor of 10 KB/s during adaptive reduction", () => {
      throttler.setLimit(15_000); // 50% = 7500, below floor
      throttler.onLatencyUpdate("profile-1", 250);

      expect(throttler.isAdaptiveReductionActive()).toBe(true);
      expect(throttler.getEffectiveLimit()).toBe(10 * 1024); // 10 KB/s floor
    });

    it("does not activate reduction when already active", () => {
      throttler.onLatencyUpdate("profile-1", 250);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);
      const effectiveAfterFirst = throttler.getEffectiveLimit();

      // Another high-latency reading should not change anything
      throttler.onLatencyUpdate("profile-1", 300);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);
      expect(throttler.getEffectiveLimit()).toBe(effectiveAfterFirst);
    });

    it("does not restore bandwidth when not in reduced state", () => {
      // Send 10 low-latency readings without prior activation
      for (let i = 0; i < 10; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }

      // Should remain at full limit (no reduction was active)
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
      expect(throttler.getEffectiveLimit()).toBe(100_000);
    });

    it("reading at exactly 149ms counts toward recovery", () => {
      throttler.onLatencyUpdate("profile-1", 250);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      for (let i = 0; i < 10; i++) {
        throttler.onLatencyUpdate("profile-1", 149);
      }
      expect(throttler.isAdaptiveReductionActive()).toBe(false);
    });

    it("reading at exactly 150ms resets recovery counter", () => {
      throttler.onLatencyUpdate("profile-1", 250);
      expect(throttler.isAdaptiveReductionActive()).toBe(true);

      for (let i = 0; i < 9; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }

      // 150ms is in the dead zone — resets counter
      throttler.onLatencyUpdate("profile-1", 150);

      // Need 10 more consecutive low readings
      for (let i = 0; i < 9; i++) {
        throttler.onLatencyUpdate("profile-1", 100);
      }
      expect(throttler.isAdaptiveReductionActive()).toBe(true);
    });
  });

  describe("destroy", () => {
    it("stops the refill timer", () => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 10_000,
        adaptiveEnabled: false,
        adaptiveFloor: 10 * 1024,
      });

      throttler.destroy();

      // Advancing time should not cause errors
      vi.advanceTimersByTime(1000);
    });

    it("flushes all pending consumers on destroy", async () => {
      throttler = new BandwidthThrottler(mockLatencyMonitor as any, {
        maxBytesPerSecond: 10_000,
        adaptiveEnabled: false,
        adaptiveFloor: 10 * 1024,
      });

      await vi.advanceTimersByTimeAsync(100);

      const resolved = vi.fn();
      throttler.consumeTokens(100_000).then(resolved);
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).not.toHaveBeenCalled();

      throttler.destroy();
      await vi.advanceTimersByTimeAsync(0);
      expect(resolved).toHaveBeenCalled();
    });
  });
});
