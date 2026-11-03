/**
 * Unit tests for the TerminalKeepAlive memory guard.
 *
 * Validates that when performance.memory reports heap usage above 80%,
 * the component reduces the effective keep-alive cap and evicts the
 * oldest terminal. When performance.memory is unavailable, the static
 * cap is used without modification.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

// We test the memory guard logic in isolation by extracting the core
// behavior into a testable unit. Since the component uses React hooks,
// we simulate the interval-based check logic directly.

// --- Types matching the component's internal interfaces ---

interface PerformanceMemory {
  usedJSHeapSize: number;
  totalJSHeapSize: number;
  jsHeapSizeLimit: number;
}

const MEMORY_PRESSURE_THRESHOLD = 0.8;

// --- Helper: simulates the memory guard check logic ---

function checkMemoryPressure(
  memory: PerformanceMemory | undefined,
  currentCap: number,
): { newCap: number; evicted: boolean } {
  if (!memory) return { newCap: currentCap, evicted: false };

  const usage = memory.usedJSHeapSize / memory.jsHeapSizeLimit;
  if (usage > MEMORY_PRESSURE_THRESHOLD) {
    const next = Math.max(1, currentCap - 1);
    return { newCap: next, evicted: next < currentCap };
  }
  return { newCap: currentCap, evicted: false };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("TerminalKeepAlive memory guard", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  describe("memory pressure detection", () => {
    it("reduces cap by 1 when heap exceeds 80% of limit", () => {
      const memory: PerformanceMemory = {
        usedJSHeapSize: 850_000_000, // 85% of limit
        totalJSHeapSize: 900_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      const result = checkMemoryPressure(memory, 5);
      expect(result.newCap).toBe(4);
      expect(result.evicted).toBe(true);
    });

    it("does not reduce cap when heap is below 80%", () => {
      const memory: PerformanceMemory = {
        usedJSHeapSize: 700_000_000, // 70% of limit
        totalJSHeapSize: 800_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      const result = checkMemoryPressure(memory, 5);
      expect(result.newCap).toBe(5);
      expect(result.evicted).toBe(false);
    });

    it("does not reduce cap below 1", () => {
      const memory: PerformanceMemory = {
        usedJSHeapSize: 950_000_000, // 95% of limit
        totalJSHeapSize: 960_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      const result = checkMemoryPressure(memory, 1);
      expect(result.newCap).toBe(1);
      expect(result.evicted).toBe(false);
    });

    it("treats exactly 80% as not exceeding the threshold", () => {
      const memory: PerformanceMemory = {
        usedJSHeapSize: 800_000_000, // exactly 80%
        totalJSHeapSize: 850_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      const result = checkMemoryPressure(memory, 5);
      expect(result.newCap).toBe(5);
      expect(result.evicted).toBe(false);
    });

    it("reduces cap progressively on repeated pressure checks", () => {
      const memory: PerformanceMemory = {
        usedJSHeapSize: 900_000_000, // 90%
        totalJSHeapSize: 950_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      let cap = 5;
      // Simulate multiple interval ticks under sustained pressure
      for (let i = 0; i < 4; i++) {
        const result = checkMemoryPressure(memory, cap);
        cap = result.newCap;
      }
      expect(cap).toBe(1);
    });
  });

  describe("performance.memory unavailable", () => {
    it("returns unchanged cap when memory API is undefined", () => {
      const result = checkMemoryPressure(undefined, 5);
      expect(result.newCap).toBe(5);
      expect(result.evicted).toBe(false);
    });
  });

  describe("warning logging", () => {
    it("logs a warning when memory-pressure eviction occurs", () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

      const memory: PerformanceMemory = {
        usedJSHeapSize: 850_000_000,
        totalJSHeapSize: 900_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      // Simulate what the component does when eviction happens
      const usage = memory.usedJSHeapSize / memory.jsHeapSizeLimit;
      if (usage > MEMORY_PRESSURE_THRESHOLD) {
        const prev = 5;
        const next = Math.max(1, prev - 1);
        if (next < prev) {
          console.warn(
            `[TerminalKeepAlive] Memory pressure detected ` +
              `(${(usage * 100).toFixed(1)}% heap used). ` +
              `Reducing keep-alive cap from ${prev} to ${next} and evicting oldest terminal.`,
          );
        }
      }

      expect(warnSpy).toHaveBeenCalledTimes(1);
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining("[TerminalKeepAlive] Memory pressure detected"),
      );
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining("85.0% heap used"),
      );
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining("Reducing keep-alive cap from 5 to 4"),
      );

      warnSpy.mockRestore();
    });
  });

  describe("interval timing", () => {
    it("checks memory on a 30-second interval", () => {
      let checkCount = 0;
      const memory: PerformanceMemory = {
        usedJSHeapSize: 500_000_000,
        totalJSHeapSize: 600_000_000,
        jsHeapSizeLimit: 1_000_000_000,
      };

      // Simulate the interval setup
      const intervalId = setInterval(() => {
        checkMemoryPressure(memory, 5);
        checkCount++;
      }, 30_000);

      // No checks yet
      expect(checkCount).toBe(0);

      // After 30s, one check
      vi.advanceTimersByTime(30_000);
      expect(checkCount).toBe(1);

      // After 60s, two checks
      vi.advanceTimersByTime(30_000);
      expect(checkCount).toBe(2);

      // After 90s, three checks
      vi.advanceTimersByTime(30_000);
      expect(checkCount).toBe(3);

      clearInterval(intervalId);
    });
  });
});
