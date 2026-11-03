/**
 * Checkpoint Timer — periodic snapshot persistence for crash recovery.
 *
 * Iterates active sessions on a timer, writing terminal snapshots to
 * disk via HistoryManager. Uses adaptive frequency: 5s for sessions
 * receiving data (active), 60s for idle sessions. Provides final
 * checkpoint on before-quit and cold restore detection on launch.
 */

import type { HistoryManager } from "./history-manager";
import type { TerminalSnapshot } from "./types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Checkpoint interval for active sessions (receiving data). */
const ACTIVE_INTERVAL_MS = 5_000;

/** Checkpoint interval for idle sessions (no data since last checkpoint). */
const IDLE_INTERVAL_MS = 60_000;


// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Provides snapshot data for a session. */
export interface SnapshotProvider {
  getSnapshot(sessionId: string): TerminalSnapshot | null;
}

/** Tracks which sessions are active (receiving data). */
export interface SessionActivityTracker {
  /** Returns all session IDs currently tracked. */
  getTrackedSessionIds(): string[];
  /** Returns true if the session has received data since the last checkpoint. */
  hasActivity(sessionId: string): boolean;
  /** Clear the activity flag for a session (called after checkpoint). */
  clearActivity(sessionId: string): void;
}

export interface CheckpointTimerOptions {
  /** HistoryManager instance for writing checkpoints. */
  historyManager: HistoryManager;
  /** Provides terminal snapshots for sessions. */
  snapshotProvider: SnapshotProvider;
  /** Tracks session activity for adaptive frequency. */
  activityTracker: SessionActivityTracker;
  /** Override active interval for testing. */
  activeIntervalMs?: number;
  /** Override idle interval for testing. */
  idleIntervalMs?: number;
}

export interface ColdRestoreResult {
  sessionId: string;
  snapshot: TerminalSnapshot;
}

// ---------------------------------------------------------------------------
// CheckpointTimer
// ---------------------------------------------------------------------------

export class CheckpointTimer {
  private readonly historyManager: HistoryManager;
  private readonly snapshotProvider: SnapshotProvider;
  private readonly activityTracker: SessionActivityTracker;
  private readonly activeIntervalMs: number;
  private readonly idleIntervalMs: number;

  private timer: ReturnType<typeof setInterval> | null = null;
  private lastCheckpointTime: Map<string, number> = new Map();
  private running = false;

  constructor(opts: CheckpointTimerOptions) {
    this.historyManager = opts.historyManager;
    this.snapshotProvider = opts.snapshotProvider;
    this.activityTracker = opts.activityTracker;
    this.activeIntervalMs = opts.activeIntervalMs ?? ACTIVE_INTERVAL_MS;
    this.idleIntervalMs = opts.idleIntervalMs ?? IDLE_INTERVAL_MS;
  }

  /**
   * Start the checkpoint timer. Ticks at the active interval rate,
   * but only checkpoints idle sessions at the idle interval rate.
   */
  start(): void {
    if (this.running) return;
    this.running = true;

    const tickInterval = this.activeIntervalMs;
    this.timer = setInterval(() => this.tick(), tickInterval);

    // Don't prevent process exit
    if (this.timer.unref) {
      this.timer.unref();
    }
  }

  /**
   * Stop the checkpoint timer.
   */
  stop(): void {
    this.running = false;
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /**
   * Whether the timer is currently running.
   */
  get isRunning(): boolean {
    return this.running;
  }

  /**
   * Perform a final checkpoint for all tracked sessions.
   * Called on before-quit to ensure latest state is persisted.
   */
  finalCheckpoint(): void {
    const sessionIds = this.activityTracker.getTrackedSessionIds();

    for (const sessionId of sessionIds) {
      this.checkpointSession(sessionId, true);
    }
  }

  /**
   * Detect cold restore for a set of session IDs.
   * Returns sessions that have unclean history files (daemon crashed
   * while app was disconnected).
   */
  detectColdRestores(sessionIds: string[]): ColdRestoreResult[] {
    const results: ColdRestoreResult[] = [];

    for (const sessionId of sessionIds) {
      const snapshot = this.historyManager.detectColdRestore(sessionId);
      if (snapshot) {
        results.push({ sessionId, snapshot });
      }
    }

    return results;
  }

  /**
   * Force a checkpoint tick (for testing). Runs regardless of timer state.
   */
  forceTick(): void {
    this.doTick();
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private tick(): void {
    if (!this.running) return;
    this.doTick();
  }

  private doTick(): void {
    const sessionIds = this.activityTracker.getTrackedSessionIds();
    const now = Date.now();

    for (const sessionId of sessionIds) {
      const hasActivity = this.activityTracker.hasActivity(sessionId);
      const lastCheckpoint = this.lastCheckpointTime.get(sessionId);

      // If we've never checkpointed this session, use the appropriate
      // interval from "now" — treat it as if the last checkpoint was
      // at (now - 0), so it won't fire until the full interval elapses.
      if (lastCheckpoint === undefined) {
        this.lastCheckpointTime.set(sessionId, now);
        // For active sessions, checkpoint immediately on first tick
        if (hasActivity) {
          this.checkpointSession(sessionId, false);
        }
        continue;
      }

      const elapsed = now - lastCheckpoint;

      if (hasActivity) {
        // Active session — checkpoint at active interval
        if (elapsed >= this.activeIntervalMs) {
          this.checkpointSession(sessionId, false);
        }
      } else {
        // Idle session — checkpoint at idle interval
        if (elapsed >= this.idleIntervalMs) {
          this.checkpointSession(sessionId, false);
        }
      }
    }
  }

  private checkpointSession(sessionId: string, force: boolean): void {
    const snapshot = this.snapshotProvider.getSnapshot(sessionId);
    if (!snapshot) return;

    // For forced checkpoints (before-quit), always mark dirty so
    // HistoryManager writes regardless of dirty-version state
    if (force) {
      this.historyManager.markDirty(sessionId);
    }

    this.historyManager.checkpoint(sessionId, snapshot);
    this.lastCheckpointTime.set(sessionId, Date.now());

    // Clear activity flag after checkpoint
    this.activityTracker.clearActivity(sessionId);
  }

  /**
   * Remove tracking state for a session that has been closed.
   */
  removeSession(sessionId: string): void {
    this.lastCheckpointTime.delete(sessionId);
  }

  /**
   * Dispose the timer and clear all state.
   */
  dispose(): void {
    this.stop();
    this.lastCheckpointTime.clear();
  }
}
