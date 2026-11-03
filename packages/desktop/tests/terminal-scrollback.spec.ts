import { describe, it, expect } from "vitest";
import {
  estimateMemory,
  validateScrollbackConfig,
  DEFAULT_MAX_LINES,
  DEFAULT_MEMORY_CAP,
  DEFAULT_OVERSCAN_LINES,
  DEFAULT_AVG_CHARS_PER_LINE,
  MIN_MAX_LINES,
  MAX_MAX_LINES,
  ScrollbackConfig,
} from "../src/lib/terminal-scrollback";

describe("estimateMemory", () => {
  it("calculates memory as lineCount × avgCharsPerLine × 2", () => {
    expect(estimateMemory(1000, 80)).toBe(1000 * 80 * 2);
  });

  it("uses default avgCharsPerLine of 80 when not provided", () => {
    expect(estimateMemory(500)).toBe(500 * 80 * 2);
  });

  it("returns 0 for zero lines", () => {
    expect(estimateMemory(0)).toBe(0);
  });

  it("returns 0 for negative lineCount", () => {
    expect(estimateMemory(-100)).toBe(0);
  });

  it("returns 0 for negative avgCharsPerLine", () => {
    expect(estimateMemory(100, -10)).toBe(0);
  });

  it("handles large line counts", () => {
    expect(estimateMemory(1_000_000, 80)).toBe(1_000_000 * 80 * 2);
  });

  it("is monotonically increasing with line count", () => {
    const a = estimateMemory(100);
    const b = estimateMemory(200);
    const c = estimateMemory(300);
    expect(b).toBeGreaterThan(a);
    expect(c).toBeGreaterThan(b);
  });
});

describe("validateScrollbackConfig", () => {
  it("returns defaults for empty config", () => {
    const result = validateScrollbackConfig({});
    expect(result.valid).toBe(true);
    expect(result.errors).toHaveLength(0);
    expect(result.normalized).toEqual({
      maxLines: DEFAULT_MAX_LINES,
      memoryCapBytes: DEFAULT_MEMORY_CAP,
      virtualScrollbackEnabled: false,
      overscanLines: DEFAULT_OVERSCAN_LINES,
    });
  });

  it("accepts valid config within bounds", () => {
    const config: ScrollbackConfig = {
      maxLines: 10_000,
      memoryCapBytes: 128 * 1024 * 1024,
      virtualScrollbackEnabled: true,
      overscanLines: 50,
    };
    const result = validateScrollbackConfig(config);
    expect(result.valid).toBe(true);
    expect(result.errors).toHaveLength(0);
    expect(result.normalized).toEqual(config);
  });

  it("clamps maxLines below minimum to 1000", () => {
    const result = validateScrollbackConfig({ maxLines: 500 });
    expect(result.valid).toBe(false);
    expect(result.normalized.maxLines).toBe(MIN_MAX_LINES);
    expect(result.errors[0]).toContain("below minimum");
  });

  it("clamps maxLines above maximum to 1000000", () => {
    const result = validateScrollbackConfig({ maxLines: 2_000_000 });
    expect(result.valid).toBe(false);
    expect(result.normalized.maxLines).toBe(MAX_MAX_LINES);
    expect(result.errors[0]).toContain("exceeds maximum");
  });

  it("rounds fractional maxLines", () => {
    const result = validateScrollbackConfig({ maxLines: 5000.7 });
    expect(result.valid).toBe(true);
    expect(result.normalized.maxLines).toBe(5001);
  });

  it("rejects NaN maxLines and uses default", () => {
    const result = validateScrollbackConfig({ maxLines: NaN });
    expect(result.valid).toBe(false);
    expect(result.normalized.maxLines).toBe(DEFAULT_MAX_LINES);
    expect(result.errors[0]).toContain("must be a valid number");
  });

  it("rejects non-positive memoryCapBytes", () => {
    const result = validateScrollbackConfig({ memoryCapBytes: 0 });
    expect(result.valid).toBe(false);
    expect(result.normalized.memoryCapBytes).toBe(DEFAULT_MEMORY_CAP);
    expect(result.errors[0]).toContain("must be positive");
  });

  it("rejects negative memoryCapBytes", () => {
    const result = validateScrollbackConfig({ memoryCapBytes: -1024 });
    expect(result.valid).toBe(false);
    expect(result.normalized.memoryCapBytes).toBe(DEFAULT_MEMORY_CAP);
    expect(result.errors[0]).toContain("must be positive");
  });

  it("rejects NaN memoryCapBytes", () => {
    const result = validateScrollbackConfig({ memoryCapBytes: NaN });
    expect(result.valid).toBe(false);
    expect(result.normalized.memoryCapBytes).toBe(DEFAULT_MEMORY_CAP);
  });

  it("rejects negative overscanLines and clamps to 0", () => {
    const result = validateScrollbackConfig({ overscanLines: -50 });
    expect(result.valid).toBe(false);
    expect(result.normalized.overscanLines).toBe(0);
    expect(result.errors[0]).toContain("non-negative");
  });

  it("rounds fractional overscanLines", () => {
    const result = validateScrollbackConfig({ overscanLines: 75.3 });
    expect(result.valid).toBe(true);
    expect(result.normalized.overscanLines).toBe(75);
  });

  it("rejects NaN overscanLines", () => {
    const result = validateScrollbackConfig({ overscanLines: NaN });
    expect(result.valid).toBe(false);
    expect(result.normalized.overscanLines).toBe(DEFAULT_OVERSCAN_LINES);
  });

  it("collects multiple errors", () => {
    const result = validateScrollbackConfig({
      maxLines: -1,
      memoryCapBytes: 0,
      overscanLines: -10,
    });
    expect(result.valid).toBe(false);
    expect(result.errors.length).toBe(3);
  });

  it("accepts boundary value maxLines = 1000", () => {
    const result = validateScrollbackConfig({ maxLines: MIN_MAX_LINES });
    expect(result.valid).toBe(true);
    expect(result.normalized.maxLines).toBe(MIN_MAX_LINES);
  });

  it("accepts boundary value maxLines = 1000000", () => {
    const result = validateScrollbackConfig({ maxLines: MAX_MAX_LINES });
    expect(result.valid).toBe(true);
    expect(result.normalized.maxLines).toBe(MAX_MAX_LINES);
  });

  it("accepts overscanLines = 0", () => {
    const result = validateScrollbackConfig({ overscanLines: 0 });
    expect(result.valid).toBe(true);
    expect(result.normalized.overscanLines).toBe(0);
  });
});
