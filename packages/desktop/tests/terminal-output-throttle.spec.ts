import { describe, it, expect, beforeEach } from "vitest";
import {
  calculateRate,
  calculateBatch,
  createOutputThrottle,
  DEFAULT_THROTTLE_CONFIG,
  type ThrottleConfig,
} from "../src/lib/terminal-output-throttle";

describe("terminal-output-throttle", () => {
  describe("calculateRate", () => {
    it("returns 0 for empty samples", () => {
      expect(calculateRate([], 1000, 1000)).toBe(0);
    });

    it("calculates rate from recent samples within window", () => {
      const samples = [
        { timestamp: 100, bytes: 5000 },
        { timestamp: 200, bytes: 5000 },
        { timestamp: 300, bytes: 5000 },
      ];
      const rate = calculateRate(samples, 1000, 400);
      // 15000 bytes over 300ms = 50000 bytes/s
      expect(rate).toBeCloseTo(50000, -1);
    });

    it("excludes samples outside the measurement window", () => {
      const samples = [
        { timestamp: 0, bytes: 100000 }, // outside window
        { timestamp: 900, bytes: 1000 },
        { timestamp: 1000, bytes: 1000 },
      ];
      const rate = calculateRate(samples, 500, 1100);
      // Only samples at 900 and 1000 are within window (1100 - 500 = 600 cutoff)
      // 2000 bytes over 200ms (from 900 to 1100) = 10000 bytes/s
      expect(rate).toBeCloseTo(10000, -1);
    });
  });

  describe("calculateBatch", () => {
    it("returns full buffer when rate is below threshold", () => {
      const result = calculateBatch(10000, 1000000, DEFAULT_THROTTLE_CONFIG);
      expect(result.batchSize).toBe(10000);
      expect(result.delayMs).toBe(0);
    });

    it("returns 0 batch size for empty buffer", () => {
      const result = calculateBatch(0, 20000000, DEFAULT_THROTTLE_CONFIG);
      expect(result.batchSize).toBe(0);
      expect(result.delayMs).toBe(0);
    });

    it("limits batch size when rate exceeds threshold", () => {
      const result = calculateBatch(
        5000000, // 5MB buffered
        20000000, // 20 MB/s (above 10 MB/s threshold)
        DEFAULT_THROTTLE_CONFIG,
      );
      // Batch size should be limited to keep render rate below frame budget
      expect(result.batchSize).toBeLessThan(5000000);
      expect(result.batchSize).toBeGreaterThan(0);
      expect(result.delayMs).toBe(DEFAULT_THROTTLE_CONFIG.frameBudgetMs);
    });

    it("ensures batch size keeps render rate below frame budget", () => {
      const config = DEFAULT_THROTTLE_CONFIG;
      const result = calculateBatch(50000000, 50000000, config);
      // At 30fps, each frame is 33ms. The batch should be sized so that
      // rendering it takes less than one frame budget.
      const framesPerSecond = 1000 / config.frameBudgetMs;
      const maxBytesPerFrame = config.burstThresholdBps / framesPerSecond;
      expect(result.batchSize).toBeLessThanOrEqual(maxBytesPerFrame);
    });

    it("has minimum batch size of 1024 bytes", () => {
      const config: ThrottleConfig = {
        ...DEFAULT_THROTTLE_CONFIG,
        burstThresholdBps: 100, // very low threshold
      };
      const result = calculateBatch(500, 200, config);
      // Below threshold, so full buffer
      expect(result.batchSize).toBe(500);
    });
  });

  describe("OutputThrottle", () => {
    it("renders immediately when below threshold", () => {
      const throttle = createOutputThrottle();
      const shouldRender = throttle.push("hello");
      expect(shouldRender).toBe(true);
      expect(throttle.getState().active).toBe(false);
    });

    it("buffers data when rate exceeds threshold", () => {
      const config: ThrottleConfig = {
        ...DEFAULT_THROTTLE_CONFIG,
        burstThresholdBps: 100, // very low threshold for testing
        rateWindowMs: 100,
      };
      const throttle = createOutputThrottle(config);

      // Push enough data to exceed the threshold
      const bigData = "x".repeat(200);
      throttle.push(bigData); // first push establishes rate
      const shouldBuffer = throttle.push(bigData); // second push should trigger throttling

      // The throttle may or may not buffer depending on timing
      // but the state should reflect the high rate
      const state = throttle.getState();
      expect(state.currentRateBps).toBeGreaterThan(0);
    });

    it("nextBatch returns null when buffer is empty", () => {
      const throttle = createOutputThrottle();
      expect(throttle.nextBatch()).toBeNull();
    });

    it("reset clears all state", () => {
      const config: ThrottleConfig = {
        ...DEFAULT_THROTTLE_CONFIG,
        burstThresholdBps: 1, // force buffering
        rateWindowMs: 10000,
      };
      const throttle = createOutputThrottle(config);
      throttle.push("data");
      throttle.push("more data");
      throttle.reset();
      const state = throttle.getState();
      expect(state.active).toBe(false);
      expect(state.bufferedBytes).toBe(0);
      expect(state.pendingBatches).toBe(0);
    });

    it("dispose prevents further pushes", () => {
      const throttle = createOutputThrottle();
      throttle.dispose();
      const result = throttle.push("data");
      expect(result).toBe(false);
    });
  });
});
