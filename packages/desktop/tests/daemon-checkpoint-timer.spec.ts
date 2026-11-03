import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import {
  CheckpointTimer,
  type SnapshotProvider,
  type SessionActivityTracker,
} from "../electron/daemon/checkpoint-timer";
import { HistoryManager } from "../electron/daemon/history-manager";
import type { TerminalSnapshot } from "../electron/daemon/types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createTmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "checkpoint-timer-test-"));
}

function makeSnapshot(overrides?: Partial<TerminalSnapshot>): TerminalSnapshot {
  return {
    snapshotAnsi: "$ hello world\r\n",
    scrollbackAnsi: "",
    rehydrateSequences: "",
    cwd: "/home/user",
    modes: {
      bracketedPaste: false,
      mouseTracking: false,
      mouseTrackingMode: "none",
      sgrMouseMode: false,
      applicationCursor: false,
      alternateScreen: false,
    },
    cols: 80,
    rows: 24,
    scrollbackLines: 0,
    ...overrides,
  };
}

/**
 * Fake activity tracker for testing.
 */
class FakeActivityTracker implements SessionActivityTracker {
  private sessions: Set<string> = new Set();
  private active: Set<string> = new Set();

  addSession(id: string, isActive = false): void {
    this.sessions.add(id);
    if (isActive) this.active.add(id);
  }

  removeSession(id: string): void {
    this.sessions.delete(id);
    this.active.delete(id);
  }

  setActive(id: string, active: boolean): void {
    if (active) {
      this.active.add(id);
    } else {
      this.active.delete(id);
    }
  }

  getTrackedSessionIds(): string[] {
    return [...this.sessions];
  }

  hasActivity(sessionId: string): boolean {
    return this.active.has(sessionId);
  }

  clearActivity(sessionId: string): void {
    this.active.delete(sessionId);
  }
}

/**
 * Fake snapshot provider for testing.
 */
class FakeSnapshotProvider implements SnapshotProvider {
  private snapshots: Map<string, TerminalSnapshot> = new Map();

  setSnapshot(sessionId: string, snapshot: TerminalSnapshot): void {
    this.snapshots.set(sessionId, snapshot);
  }

  removeSnapshot(sessionId: string): void {
    this.snapshots.delete(sessionId);
  }

  getSnapshot(sessionId: string): TerminalSnapshot | null {
    return this.snapshots.get(sessionId) ?? null;
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("CheckpointTimer", () => {
  let tmpDir: string;
  let historyManager: HistoryManager;
  let snapshotProvider: FakeSnapshotProvider;
  let activityTracker: FakeActivityTracker;

  beforeEach(() => {
    tmpDir = createTmpDir();
    historyManager = new HistoryManager({ basePath: tmpDir });
    snapshotProvider = new FakeSnapshotProvider();
    activityTracker = new FakeActivityTracker();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  // -------------------------------------------------------------------------
  // Timer lifecycle
  // -------------------------------------------------------------------------

  describe("timer lifecycle", () => {
    it("starts and stops the timer", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
      });

      expect(timer.isRunning).toBe(false);
      timer.start();
      expect(timer.isRunning).toBe(true);
      timer.stop();
      expect(timer.isRunning).toBe(false);
    });

    it("start is idempotent", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
      });

      timer.start();
      timer.start(); // second call is a no-op
      expect(timer.isRunning).toBe(true);
      timer.stop();
    });

    it("dispose stops the timer and clears state", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
      });

      timer.start();
      timer.dispose();
      expect(timer.isRunning).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // Adaptive frequency
  // -------------------------------------------------------------------------

  describe("adaptive frequency", () => {
    it("checkpoints active sessions at 5s interval", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      // Set up session
      historyManager.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("sess-1");
      activityTracker.addSession("sess-1", true);
      snapshotProvider.setSnapshot("sess-1", makeSnapshot({ cwd: "/active" }));

      timer.start();

      // Advance 5s — should checkpoint
      vi.advanceTimersByTime(5_000);

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);
      const data = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(data.snapshot.cwd).toBe("/active");

      timer.dispose();
    });

    it("does not checkpoint idle sessions at 5s interval", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      // Set up idle session
      historyManager.openSession("sess-idle", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("sess-idle");
      activityTracker.addSession("sess-idle", false); // idle
      snapshotProvider.setSnapshot("sess-idle", makeSnapshot({ cwd: "/idle" }));

      timer.start();

      // Advance 5s — should NOT checkpoint (idle)
      vi.advanceTimersByTime(5_000);

      const checkpointPath = path.join(tmpDir, "sess-idle", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);

      timer.dispose();
    });

    it("checkpoints idle sessions at 60s interval", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      // Set up idle session
      historyManager.openSession("sess-idle", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("sess-idle");
      activityTracker.addSession("sess-idle", false); // idle
      snapshotProvider.setSnapshot("sess-idle", makeSnapshot({ cwd: "/idle" }));

      timer.start();

      // First tick at 5s registers the session (sets lastCheckpointTime)
      // Then 60s must elapse from that point for idle checkpoint
      vi.advanceTimersByTime(5_000);
      const checkpointPath = path.join(tmpDir, "sess-idle", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);

      // Advance another 60s — should checkpoint idle session
      vi.advanceTimersByTime(60_000);
      expect(fs.existsSync(checkpointPath)).toBe(true);
      const data = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(data.snapshot.cwd).toBe("/idle");

      timer.dispose();
    });

    it("transitions from active to idle frequency when activity stops", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      historyManager.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("sess-1");
      activityTracker.addSession("sess-1", true);
      snapshotProvider.setSnapshot("sess-1", makeSnapshot({ cwd: "/v1" }));

      timer.start();

      // First tick at 5s — checkpoints (active, first encounter)
      vi.advanceTimersByTime(5_000);
      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);

      // Activity was cleared by checkpoint. Session is now idle.
      // Mark dirty again for next potential checkpoint
      historyManager.markDirty("sess-1");
      snapshotProvider.setSnapshot("sess-1", makeSnapshot({ cwd: "/v2" }));

      // Advance another 5s — should NOT checkpoint (now idle, needs 60s from last checkpoint)
      vi.advanceTimersByTime(5_000);
      const data1 = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(data1.snapshot.cwd).toBe("/v1"); // unchanged

      // Advance to 60s since last checkpoint — should checkpoint
      vi.advanceTimersByTime(55_000);
      const data2 = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(data2.snapshot.cwd).toBe("/v2");

      timer.dispose();
    });
  });

  // -------------------------------------------------------------------------
  // Final checkpoint (before-quit)
  // -------------------------------------------------------------------------

  describe("finalCheckpoint", () => {
    it("checkpoints all tracked sessions regardless of activity", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
      });

      // Set up two sessions — one active, one idle
      historyManager.openSession("active-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.openSession("idle-1", { cwd: "/tmp", cols: 80, rows: 24 });
      activityTracker.addSession("active-1", true);
      activityTracker.addSession("idle-1", false);
      snapshotProvider.setSnapshot("active-1", makeSnapshot({ cwd: "/active" }));
      snapshotProvider.setSnapshot("idle-1", makeSnapshot({ cwd: "/idle" }));

      // Final checkpoint — should write both
      timer.finalCheckpoint();

      const activePath = path.join(tmpDir, "active-1", "checkpoint.json");
      const idlePath = path.join(tmpDir, "idle-1", "checkpoint.json");

      expect(fs.existsSync(activePath)).toBe(true);
      expect(fs.existsSync(idlePath)).toBe(true);

      const activeData = JSON.parse(fs.readFileSync(activePath, "utf-8"));
      const idleData = JSON.parse(fs.readFileSync(idlePath, "utf-8"));
      expect(activeData.snapshot.cwd).toBe("/active");
      expect(idleData.snapshot.cwd).toBe("/idle");
    });

    it("skips sessions with no snapshot available", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
      });

      historyManager.openSession("no-snap", { cwd: "/tmp", cols: 80, rows: 24 });
      activityTracker.addSession("no-snap", true);
      // No snapshot set in provider

      // Should not throw
      timer.finalCheckpoint();

      const checkpointPath = path.join(tmpDir, "no-snap", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(false);
    });
  });

  // -------------------------------------------------------------------------
  // Cold restore detection
  // -------------------------------------------------------------------------

  describe("detectColdRestores", () => {
    it("detects sessions with unclean history files", () => {
      // Simulate a previous session that crashed (no closeSession called)
      historyManager.openSession("crashed-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("crashed-1");
      historyManager.checkpoint("crashed-1", makeSnapshot({ snapshotAnsi: "recovered" }));

      // Create a new timer (simulating app relaunch)
      const newHistoryManager = new HistoryManager({ basePath: tmpDir });
      const timer = new CheckpointTimer({
        historyManager: newHistoryManager,
        snapshotProvider,
        activityTracker,
      });

      const results = timer.detectColdRestores(["crashed-1"]);

      expect(results).toHaveLength(1);
      expect(results[0].sessionId).toBe("crashed-1");
      expect(results[0].snapshot.snapshotAnsi).toBe("recovered");
    });

    it("returns empty for cleanly closed sessions", () => {
      historyManager.openSession("clean-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("clean-1");
      historyManager.checkpoint("clean-1", makeSnapshot());
      historyManager.closeSession("clean-1", 0);

      const newHistoryManager = new HistoryManager({ basePath: tmpDir });
      const timer = new CheckpointTimer({
        historyManager: newHistoryManager,
        snapshotProvider,
        activityTracker,
      });

      const results = timer.detectColdRestores(["clean-1"]);
      expect(results).toHaveLength(0);
    });

    it("returns empty for non-existent sessions", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
      });

      const results = timer.detectColdRestores(["nonexistent"]);
      expect(results).toHaveLength(0);
    });

    it("handles multiple sessions with mixed states", () => {
      // Crashed session
      historyManager.openSession("crashed", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("crashed");
      historyManager.checkpoint("crashed", makeSnapshot({ cwd: "/crashed" }));

      // Clean session
      historyManager.openSession("clean", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("clean");
      historyManager.checkpoint("clean", makeSnapshot());
      historyManager.closeSession("clean", 0);

      const newHistoryManager = new HistoryManager({ basePath: tmpDir });
      const timer = new CheckpointTimer({
        historyManager: newHistoryManager,
        snapshotProvider,
        activityTracker,
      });

      const results = timer.detectColdRestores(["crashed", "clean", "missing"]);

      expect(results).toHaveLength(1);
      expect(results[0].sessionId).toBe("crashed");
      expect(results[0].snapshot.cwd).toBe("/crashed");
    });
  });

  // -------------------------------------------------------------------------
  // Session removal
  // -------------------------------------------------------------------------

  describe("removeSession", () => {
    it("clears tracking state for removed sessions", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      historyManager.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("sess-1");
      activityTracker.addSession("sess-1", true);
      snapshotProvider.setSnapshot("sess-1", makeSnapshot());

      timer.start();

      // Checkpoint once
      vi.advanceTimersByTime(5_000);

      // Remove session
      timer.removeSession("sess-1");
      activityTracker.removeSession("sess-1");

      // Advance time — should not crash or attempt checkpoint
      vi.advanceTimersByTime(60_000);

      timer.dispose();
    });
  });

  // -------------------------------------------------------------------------
  // forceTick
  // -------------------------------------------------------------------------

  describe("forceTick", () => {
    it("triggers immediate checkpoint for active sessions", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      historyManager.openSession("sess-1", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("sess-1");
      activityTracker.addSession("sess-1", true);
      snapshotProvider.setSnapshot("sess-1", makeSnapshot({ cwd: "/forced" }));

      // Force tick without starting the timer
      timer.forceTick();

      const checkpointPath = path.join(tmpDir, "sess-1", "checkpoint.json");
      expect(fs.existsSync(checkpointPath)).toBe(true);
      const data = JSON.parse(fs.readFileSync(checkpointPath, "utf-8"));
      expect(data.snapshot.cwd).toBe("/forced");
    });
  });

  // -------------------------------------------------------------------------
  // Multiple sessions
  // -------------------------------------------------------------------------

  describe("multiple sessions", () => {
    it("checkpoints active and idle sessions at their respective intervals", () => {
      const timer = new CheckpointTimer({
        historyManager,
        snapshotProvider,
        activityTracker,
        activeIntervalMs: 5_000,
        idleIntervalMs: 60_000,
      });

      // Active session
      historyManager.openSession("active", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("active");
      activityTracker.addSession("active", true);
      snapshotProvider.setSnapshot("active", makeSnapshot({ cwd: "/active" }));

      // Idle session
      historyManager.openSession("idle", { cwd: "/tmp", cols: 80, rows: 24 });
      historyManager.markDirty("idle");
      activityTracker.addSession("idle", false);
      snapshotProvider.setSnapshot("idle", makeSnapshot({ cwd: "/idle" }));

      timer.start();

      // At 5s: active checkpointed (first encounter + active), idle registered but not checkpointed
      vi.advanceTimersByTime(5_000);

      const activePath = path.join(tmpDir, "active", "checkpoint.json");
      const idlePath = path.join(tmpDir, "idle", "checkpoint.json");

      expect(fs.existsSync(activePath)).toBe(true);
      expect(fs.existsSync(idlePath)).toBe(false);

      // At 65s (60s after idle was first seen): idle checkpointed
      vi.advanceTimersByTime(60_000);
      expect(fs.existsSync(idlePath)).toBe(true);

      timer.dispose();
    });
  });
});
