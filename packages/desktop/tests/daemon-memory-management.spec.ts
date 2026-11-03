import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  MemoryManager,
  type ManagedSession,
  type HighThroughputCallback,
} from "../electron/daemon/memory-management";

// ---------------------------------------------------------------------------
// Mock ManagedSession
// ---------------------------------------------------------------------------

function createMockSession(overrides: Partial<ManagedSession> & { sessionId: string }): ManagedSession & {
  clearScrollbackCalled: boolean;
} {
  const session = {
    sessionId: overrides.sessionId,
    isAlive: overrides.isAlive ?? true,
    lastDataAt: overrides.lastDataAt ?? Date.now(),
    clearScrollbackCalled: false,
    getScrollbackLines: overrides.getScrollbackLines ?? (() => 3000),
    clearScrollback() {
      session.clearScrollbackCalled = true;
    },
  };
  return session;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("MemoryManager", () => {
  let manager: MemoryManager;
  let sessions: ReturnType<typeof createMockSession>[];
  let highThroughputEvents: Array<{ sessionId: string; isHighThroughput: boolean }>;
  let heapUsed: number;

  beforeEach(() => {
    vi.useFakeTimers();
    sessions = [];
    highThroughputEvents = [];
    heapUsed = 100 * 1024 * 1024; // 100 MB default (no pressure)
  });

  afterEach(() => {
    manager?.dispose();
    vi.useRealTimers();
  });

  function createManager(opts?: {
    tickIntervalMs?: number;
    idleThresholdMs?: number;
    idleTrimLines?: number;
    memoryPressureBytes?: number;
    pressureTrimLines?: number;
    highThroughputBytesPerSec?: number;
    throughputWindowSec?: number;
  }) {
    const onHighThroughput: HighThroughputCallback = (sessionId, isHigh) => {
      highThroughputEvents.push({ sessionId, isHighThroughput: isHigh });
    };

    manager = new MemoryManager(
      () => sessions,
      onHighThroughput,
      {
        tickIntervalMs: opts?.tickIntervalMs ?? 1000,
        idleThresholdMs: opts?.idleThresholdMs ?? 60 * 60 * 1000,
        idleTrimLines: opts?.idleTrimLines ?? 1000,
        memoryPressureBytes: opts?.memoryPressureBytes ?? 512 * 1024 * 1024,
        pressureTrimLines: opts?.pressureTrimLines ?? 500,
        highThroughputBytesPerSec: opts?.highThroughputBytesPerSec ?? 1024 * 1024,
        throughputWindowSec: opts?.throughputWindowSec ?? 5,
        getHeapUsed: () => heapUsed,
      },
    );
    return manager;
  }

  // -------------------------------------------------------------------------
  // Idle scrollback trimming
  // -------------------------------------------------------------------------

  describe("idle scrollback trimming", () => {
    it("trims scrollback for sessions idle longer than threshold", () => {
      createManager({ idleThresholdMs: 3600_000 });

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now() - 3600_001, // idle for just over 1 hour
        getScrollbackLines: () => 3000,
      });
      sessions.push(session);

      manager.tick();

      expect(session.clearScrollbackCalled).toBe(true);
    });

    it("does not trim sessions that are not idle long enough", () => {
      createManager({ idleThresholdMs: 3600_000 });

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now() - 1800_000, // idle for 30 minutes
        getScrollbackLines: () => 3000,
      });
      sessions.push(session);

      manager.tick();

      expect(session.clearScrollbackCalled).toBe(false);
    });

    it("does not trim idle sessions with scrollback below the limit", () => {
      createManager({ idleThresholdMs: 3600_000, idleTrimLines: 1000 });

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now() - 3600_001,
        getScrollbackLines: () => 500, // below 1000 limit
      });
      sessions.push(session);

      manager.tick();

      expect(session.clearScrollbackCalled).toBe(false);
    });

    it("does not trim dead sessions", () => {
      createManager({ idleThresholdMs: 3600_000 });

      const session = createMockSession({
        sessionId: "s1",
        isAlive: false,
        lastDataAt: Date.now() - 7200_000,
        getScrollbackLines: () => 5000,
      });
      sessions.push(session);

      manager.tick();

      expect(session.clearScrollbackCalled).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // Memory pressure detection
  // -------------------------------------------------------------------------

  describe("memory pressure detection", () => {
    it("trims all sessions when heap exceeds threshold", () => {
      createManager({ memoryPressureBytes: 512 * 1024 * 1024, pressureTrimLines: 500 });

      heapUsed = 600 * 1024 * 1024; // 600 MB — over threshold

      const s1 = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now(), // recently active
        getScrollbackLines: () => 3000,
      });
      const s2 = createMockSession({
        sessionId: "s2",
        lastDataAt: Date.now(),
        getScrollbackLines: () => 2000,
      });
      sessions.push(s1, s2);

      manager.tick();

      expect(s1.clearScrollbackCalled).toBe(true);
      expect(s2.clearScrollbackCalled).toBe(true);
    });

    it("does not trim sessions already below pressure limit", () => {
      createManager({ memoryPressureBytes: 512 * 1024 * 1024, pressureTrimLines: 500 });

      heapUsed = 600 * 1024 * 1024;

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now(),
        getScrollbackLines: () => 400, // below 500 limit
      });
      sessions.push(session);

      manager.tick();

      expect(session.clearScrollbackCalled).toBe(false);
    });

    it("memory pressure takes precedence over idle trimming", () => {
      createManager({
        memoryPressureBytes: 512 * 1024 * 1024,
        pressureTrimLines: 500,
        idleThresholdMs: 3600_000,
        idleTrimLines: 1000,
      });

      heapUsed = 600 * 1024 * 1024;

      // Session is recently active (would not be idle-trimmed)
      // but memory pressure forces aggressive trim
      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now(),
        getScrollbackLines: () => 800, // above 500 (pressure) but below 1000 (idle)
      });
      sessions.push(session);

      manager.tick();

      expect(session.clearScrollbackCalled).toBe(true);
    });

    it("does not trim when heap is below threshold", () => {
      createManager({ memoryPressureBytes: 512 * 1024 * 1024 });

      heapUsed = 200 * 1024 * 1024; // well below threshold

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now(), // recently active
        getScrollbackLines: () => 5000,
      });
      sessions.push(session);

      manager.tick();

      // Not idle, not under pressure — no trim
      expect(session.clearScrollbackCalled).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // High-throughput mode
  // -------------------------------------------------------------------------

  describe("high-throughput mode", () => {
    it("enters high-throughput mode when output exceeds threshold", () => {
      createManager({
        highThroughputBytesPerSec: 1024 * 1024, // 1 MB/s
        throughputWindowSec: 5,
      });

      const now = Date.now();
      vi.setSystemTime(now);

      // Record 6 MB within a 5-second window → 1.2 MB/s (above threshold)
      manager.recordBytes("s1", 6 * 1024 * 1024);

      // Advance past the window
      vi.setSystemTime(now + 5000);

      // Next recordBytes triggers window evaluation
      manager.recordBytes("s1", 100);

      expect(manager.isHighThroughput("s1")).toBe(true);
      expect(highThroughputEvents).toContainEqual({
        sessionId: "s1",
        isHighThroughput: true,
      });
    });

    it("exits high-throughput mode when output drops below threshold", () => {
      createManager({
        highThroughputBytesPerSec: 1024 * 1024,
        throughputWindowSec: 5,
      });

      const now = Date.now();
      vi.setSystemTime(now);

      // First window: high throughput
      manager.recordBytes("s1", 6 * 1024 * 1024);
      vi.setSystemTime(now + 5000);
      manager.recordBytes("s1", 100); // triggers evaluation → enters high throughput

      expect(manager.isHighThroughput("s1")).toBe(true);

      // Second window: low throughput
      vi.setSystemTime(now + 10_000);
      manager.recordBytes("s1", 100); // triggers evaluation → exits high throughput

      expect(manager.isHighThroughput("s1")).toBe(false);
      expect(highThroughputEvents).toContainEqual({
        sessionId: "s1",
        isHighThroughput: false,
      });
    });

    it("does not fire callback when state does not change", () => {
      createManager({
        highThroughputBytesPerSec: 1024 * 1024,
        throughputWindowSec: 5,
      });

      const now = Date.now();
      vi.setSystemTime(now);

      // Low throughput window
      manager.recordBytes("s1", 1000);
      vi.setSystemTime(now + 5000);
      manager.recordBytes("s1", 1000);

      // Still low — no callback
      expect(highThroughputEvents).toHaveLength(0);
      expect(manager.isHighThroughput("s1")).toBe(false);
    });

    it("returns false for unknown sessions", () => {
      createManager();
      expect(manager.isHighThroughput("unknown")).toBe(false);
    });

    it("removes session tracking state on removeSession", () => {
      createManager({
        highThroughputBytesPerSec: 1024 * 1024,
        throughputWindowSec: 5,
      });

      const now = Date.now();
      vi.setSystemTime(now);

      manager.recordBytes("s1", 6 * 1024 * 1024);
      vi.setSystemTime(now + 5000);
      manager.recordBytes("s1", 100);

      expect(manager.isHighThroughput("s1")).toBe(true);

      manager.removeSession("s1");
      expect(manager.isHighThroughput("s1")).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // Timer lifecycle
  // -------------------------------------------------------------------------

  describe("timer lifecycle", () => {
    it("runs tick on the configured interval", () => {
      createManager({ tickIntervalMs: 1000 });
      manager.start();

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now() - 3600_001,
        getScrollbackLines: () => 3000,
      });
      sessions.push(session);

      vi.advanceTimersByTime(1000);

      expect(session.clearScrollbackCalled).toBe(true);
    });

    it("does not run after dispose", () => {
      createManager({ tickIntervalMs: 1000 });
      manager.start();

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now() - 3600_001,
        getScrollbackLines: () => 3000,
      });
      sessions.push(session);

      manager.dispose();
      vi.advanceTimersByTime(1000);

      expect(session.clearScrollbackCalled).toBe(false);
    });

    it("start is idempotent", () => {
      createManager({ tickIntervalMs: 1000 });
      manager.start();
      manager.start(); // second call should be no-op

      const session = createMockSession({
        sessionId: "s1",
        lastDataAt: Date.now() - 3600_001,
        getScrollbackLines: () => 3000,
      });
      sessions.push(session);

      // Only one timer should fire
      vi.advanceTimersByTime(1000);
      expect(session.clearScrollbackCalled).toBe(true);
    });
  });

  // -------------------------------------------------------------------------
  // Cleanup of stale throughput state
  // -------------------------------------------------------------------------

  describe("throughput state cleanup", () => {
    it("removes throughput state for sessions that no longer exist", () => {
      createManager({
        highThroughputBytesPerSec: 1024 * 1024,
        throughputWindowSec: 5,
      });

      const now = Date.now();
      vi.setSystemTime(now);

      // Record bytes for a session
      manager.recordBytes("s1", 1000);

      // Session is not in the sessions list
      sessions = [];

      manager.tick();

      // Throughput state should be cleaned up
      expect(manager.isHighThroughput("s1")).toBe(false);
    });
  });
});
