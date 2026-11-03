/**
 * Memory management for the PTY daemon.
 *
 * Three strategies to keep daemon memory bounded:
 *
 * 1. Idle scrollback trimming — sessions idle for >1 hour get their
 *    scrollback trimmed to 1000 lines to reduce memory footprint.
 *
 * 2. Memory pressure detection — when the daemon's heap exceeds 512 MB,
 *    aggressively trim all sessions to 500 lines.
 *
 * 3. High-throughput mode — when a session produces >1 MB/s of output,
 *    skip feeding the headless emulator (too expensive) and mark the
 *    snapshot as stale. Resume normal emulator feeding when throughput
 *    drops below threshold.
 */

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** How often the memory management timer fires (60 seconds). */
const TICK_INTERVAL_MS = 60_000;

/** Sessions idle longer than this get scrollback trimmed. */
const IDLE_THRESHOLD_MS = 60 * 60 * 1000; // 1 hour

/** Scrollback line limit for idle sessions. */
const IDLE_TRIM_LINES = 1000;

/** Heap usage threshold that triggers aggressive trimming. */
const MEMORY_PRESSURE_BYTES = 512 * 1024 * 1024; // 512 MB

/** Scrollback line limit under memory pressure. */
const PRESSURE_TRIM_LINES = 500;

/** Throughput threshold for entering high-throughput mode (bytes/sec). */
const HIGH_THROUGHPUT_BYTES_PER_SEC = 1 * 1024 * 1024; // 1 MB/s

/** Window over which throughput is measured (seconds). */
const THROUGHPUT_WINDOW_SEC = 5;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Per-session throughput tracking state. */
export interface SessionThroughputState {
  /** Bytes received in the current measurement window. */
  bytesInWindow: number;
  /** Timestamp when the current window started. */
  windowStartMs: number;
  /** Whether this session is currently in high-throughput mode. */
  isHighThroughput: boolean;
}

/** Interface for a session that the memory manager can operate on. */
export interface ManagedSession {
  readonly sessionId: string;
  readonly isAlive: boolean;
  /** Timestamp of last data received from the PTY. */
  lastDataAt: number;
  /** Current scrollback line count from the emulator snapshot. */
  getScrollbackLines(): number;
  /** Clear the emulator's scrollback buffer. */
  clearScrollback(): void;
}

/** Callback to get all active sessions. */
export type GetSessionsFn = () => ManagedSession[];

/** Callback invoked when a session enters/exits high-throughput mode. */
export type HighThroughputCallback = (
  sessionId: string,
  isHighThroughput: boolean,
) => void;

export interface MemoryManagerOptions {
  /** Interval for the maintenance timer in ms. Default: 60_000. */
  tickIntervalMs?: number;
  /** Idle threshold in ms before trimming. Default: 3_600_000 (1h). */
  idleThresholdMs?: number;
  /** Scrollback limit for idle sessions. Default: 1000. */
  idleTrimLines?: number;
  /** Heap usage threshold in bytes for pressure mode. Default: 512 MB. */
  memoryPressureBytes?: number;
  /** Scrollback limit under memory pressure. Default: 500. */
  pressureTrimLines?: number;
  /** Throughput threshold in bytes/sec. Default: 1 MB/s. */
  highThroughputBytesPerSec?: number;
  /** Throughput measurement window in seconds. Default: 5. */
  throughputWindowSec?: number;
  /** Function to get current heap usage. Default: process.memoryUsage().heapUsed. */
  getHeapUsed?: () => number;
}

// ---------------------------------------------------------------------------
// MemoryManager
// ---------------------------------------------------------------------------

export class MemoryManager {
  private readonly getSessions: GetSessionsFn;
  private readonly onHighThroughput: HighThroughputCallback;
  private readonly tickIntervalMs: number;
  private readonly idleThresholdMs: number;
  private readonly idleTrimLines: number;
  private readonly memoryPressureBytes: number;
  private readonly pressureTrimLines: number;
  private readonly highThroughputBytesPerSec: number;
  private readonly throughputWindowSec: number;
  private readonly getHeapUsed: () => number;

  private timer: ReturnType<typeof setInterval> | null = null;
  private throughputState: Map<string, SessionThroughputState> = new Map();
  private disposed = false;

  constructor(
    getSessions: GetSessionsFn,
    onHighThroughput: HighThroughputCallback,
    opts?: MemoryManagerOptions,
  ) {
    this.getSessions = getSessions;
    this.onHighThroughput = onHighThroughput;
    this.tickIntervalMs = opts?.tickIntervalMs ?? TICK_INTERVAL_MS;
    this.idleThresholdMs = opts?.idleThresholdMs ?? IDLE_THRESHOLD_MS;
    this.idleTrimLines = opts?.idleTrimLines ?? IDLE_TRIM_LINES;
    this.memoryPressureBytes = opts?.memoryPressureBytes ?? MEMORY_PRESSURE_BYTES;
    this.pressureTrimLines = opts?.pressureTrimLines ?? PRESSURE_TRIM_LINES;
    this.highThroughputBytesPerSec = opts?.highThroughputBytesPerSec ?? HIGH_THROUGHPUT_BYTES_PER_SEC;
    this.throughputWindowSec = opts?.throughputWindowSec ?? THROUGHPUT_WINDOW_SEC;
    this.getHeapUsed = opts?.getHeapUsed ?? (() => process.memoryUsage().heapUsed);
  }

  /**
   * Start the periodic memory management timer.
   */
  start(): void {
    if (this.disposed || this.timer !== null) return;

    this.timer = setInterval(() => this.tick(), this.tickIntervalMs);
    // Unref so the timer doesn't keep the process alive
    if (this.timer.unref) {
      this.timer.unref();
    }
  }

  /**
   * Stop the timer and clean up.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    this.throughputState.clear();
  }

  /**
   * Record bytes received for a session. Called on every PTY data chunk
   * to track throughput for high-throughput mode detection.
   */
  recordBytes(sessionId: string, byteCount: number): void {
    if (this.disposed) return;

    const now = Date.now();
    let state = this.throughputState.get(sessionId);

    if (!state) {
      state = {
        bytesInWindow: 0,
        windowStartMs: now,
        isHighThroughput: false,
      };
      this.throughputState.set(sessionId, state);
    }

    const windowMs = this.throughputWindowSec * 1000;
    const elapsed = now - state.windowStartMs;

    if (elapsed >= windowMs) {
      // Window expired — evaluate throughput and start a new window
      const bytesPerSec = state.bytesInWindow / (elapsed / 1000);
      const wasHighThroughput = state.isHighThroughput;
      state.isHighThroughput = bytesPerSec >= this.highThroughputBytesPerSec;

      if (state.isHighThroughput !== wasHighThroughput) {
        this.onHighThroughput(sessionId, state.isHighThroughput);
      }

      // Reset window
      state.bytesInWindow = byteCount;
      state.windowStartMs = now;
    } else {
      state.bytesInWindow += byteCount;
    }
  }

  /**
   * Check if a session is currently in high-throughput mode.
   */
  isHighThroughput(sessionId: string): boolean {
    return this.throughputState.get(sessionId)?.isHighThroughput ?? false;
  }

  /**
   * Remove tracking state for a session (call on session exit).
   */
  removeSession(sessionId: string): void {
    this.throughputState.delete(sessionId);
  }

  /**
   * Run one maintenance tick. Exposed for testing.
   */
  tick(): void {
    if (this.disposed) return;

    const heapUsed = this.getHeapUsed();
    const sessions = this.getSessions();
    const now = Date.now();

    if (heapUsed > this.memoryPressureBytes) {
      // Memory pressure: aggressively trim all sessions
      this.trimAllSessions(sessions, this.pressureTrimLines);
    } else {
      // Normal: trim only idle sessions
      this.trimIdleSessions(sessions, now);
    }

    // Clean up throughput state for sessions that no longer exist
    const activeIds = new Set(sessions.map((s) => s.sessionId));
    for (const id of this.throughputState.keys()) {
      if (!activeIds.has(id)) {
        this.throughputState.delete(id);
      }
    }
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private trimIdleSessions(sessions: ManagedSession[], now: number): void {
    for (const session of sessions) {
      if (!session.isAlive) continue;

      const idleMs = now - session.lastDataAt;
      if (idleMs > this.idleThresholdMs) {
        const scrollbackLines = session.getScrollbackLines();
        if (scrollbackLines > this.idleTrimLines) {
          session.clearScrollback();
        }
      }
    }
  }

  private trimAllSessions(sessions: ManagedSession[], maxLines: number): void {
    for (const session of sessions) {
      if (!session.isAlive) continue;

      const scrollbackLines = session.getScrollbackLines();
      if (scrollbackLines > maxLines) {
        session.clearScrollback();
      }
    }
  }
}
