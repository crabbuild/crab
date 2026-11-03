/**
 * DaemonMetrics — in-memory metrics tracking for the PTY daemon.
 *
 * Tracks:
 * - Uptime
 * - Session count (active + total spawned + peak)
 * - Client count (connections + disconnections)
 * - Memory usage (heap)
 * - File descriptor count (refreshed on a 60s timer)
 * - Total bytes relayed through stream sockets
 * - Checkpoint stats (written + failures)
 * - Respawn count
 * - Last client connected timestamp
 *
 * Exposes a snapshot via `getMetrics()` for the `getMetrics` RPC and
 * the `crab:daemon-metrics` IPC handler.
 */

import * as fs from "node:fs";
import { execFile } from "node:child_process";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface DaemonMetricsSnapshot {
  uptimeMs: number;
  totalSessionsSpawned: number;
  activeSessionCount: number;
  peakSessionCount: number;
  totalBytesRelayed: number;
  totalCheckpointsWritten: number;
  checkpointFailures: number;
  clientConnections: number;
  clientDisconnections: number;
  respawnCount: number;
  lastClientConnectedAt: number | null;
  memoryUsageBytes: number;
  fdCount: number;
}

export interface DaemonMetricsOptions {
  /** Interval in ms for refreshing the fd count. Default: 60_000 (60s). */
  fdPollIntervalMs?: number;
  /** Override for getting active session count (injected from TerminalHost). */
  getActiveSessionCount?: () => number;
}

// ---------------------------------------------------------------------------
// fd count helpers
// ---------------------------------------------------------------------------

/**
 * Read the current process's open file descriptor count.
 * On Linux, reads /proc/self/fd. On macOS, uses lsof.
 * Returns -1 if the count cannot be determined.
 */
export async function getFdCount(): Promise<number> {
  if (process.platform === "linux") {
    return getFdCountLinux();
  }
  if (process.platform === "darwin") {
    return getFdCountMacOS();
  }
  return -1;
}

function getFdCountLinux(): Promise<number> {
  return new Promise<number>((resolve) => {
    const fdDir = `/proc/${process.pid}/fd`;
    fs.readdir(fdDir, (err, files) => {
      if (err) {
        resolve(-1);
        return;
      }
      resolve(files.length);
    });
  });
}

function getFdCountMacOS(): Promise<number> {
  return new Promise<number>((resolve) => {
    execFile("lsof", ["-p", String(process.pid)], { timeout: 5000 }, (err, stdout) => {
      if (err) {
        resolve(-1);
        return;
      }
      // lsof outputs one line per fd, minus the header line
      const lines = stdout.trim().split("\n");
      resolve(Math.max(0, lines.length - 1));
    });
  });
}

// ---------------------------------------------------------------------------
// DaemonMetrics
// ---------------------------------------------------------------------------

export class DaemonMetrics {
  private readonly startedAt: number = Date.now();
  private totalSessionsSpawned = 0;
  private peakSessionCount = 0;
  private totalBytesRelayed = 0;
  private totalCheckpointsWritten = 0;
  private checkpointFailures = 0;
  private clientConnections = 0;
  private clientDisconnections = 0;
  private respawnCount = 0;
  private lastClientConnectedAt: number | null = null;
  private fdCount = -1;
  private fdPollTimer: ReturnType<typeof setInterval> | null = null;
  private disposed = false;

  private readonly getActiveSessionCount: () => number;

  constructor(opts?: DaemonMetricsOptions) {
    const fdPollIntervalMs = opts?.fdPollIntervalMs ?? 60_000;
    this.getActiveSessionCount = opts?.getActiveSessionCount ?? (() => 0);

    // Initial fd count poll
    void this.pollFdCount();

    // Start periodic fd count polling
    if (fdPollIntervalMs > 0) {
      this.fdPollTimer = setInterval(() => {
        void this.pollFdCount();
      }, fdPollIntervalMs);

      // Unref so it doesn't keep the process alive
      if (this.fdPollTimer.unref) {
        this.fdPollTimer.unref();
      }
    }
  }

  // -------------------------------------------------------------------------
  // Recording methods (called by DaemonServer / TerminalHost)
  // -------------------------------------------------------------------------

  /** Record that a new session was spawned. */
  recordSessionSpawned(): void {
    this.totalSessionsSpawned++;
    const active = this.getActiveSessionCount();
    if (active > this.peakSessionCount) {
      this.peakSessionCount = active;
    }
  }

  /** Record bytes relayed through stream sockets. */
  recordBytesRelayed(bytes: number): void {
    this.totalBytesRelayed += bytes;
  }

  /** Record a successful checkpoint write. */
  recordCheckpointWritten(): void {
    this.totalCheckpointsWritten++;
  }

  /** Record a failed checkpoint write. */
  recordCheckpointFailure(): void {
    this.checkpointFailures++;
  }

  /** Record a client connection. */
  recordClientConnected(): void {
    this.clientConnections++;
    this.lastClientConnectedAt = Date.now();
  }

  /** Record a client disconnection. */
  recordClientDisconnected(): void {
    this.clientDisconnections++;
  }

  /** Record a daemon respawn event. */
  recordRespawn(): void {
    this.respawnCount++;
  }

  /** Update peak session count (call when session count changes). */
  updatePeakSessionCount(): void {
    const active = this.getActiveSessionCount();
    if (active > this.peakSessionCount) {
      this.peakSessionCount = active;
    }
  }

  // -------------------------------------------------------------------------
  // Snapshot
  // -------------------------------------------------------------------------

  /** Get the current metrics snapshot. */
  getMetrics(): DaemonMetricsSnapshot {
    return {
      uptimeMs: Date.now() - this.startedAt,
      totalSessionsSpawned: this.totalSessionsSpawned,
      activeSessionCount: this.getActiveSessionCount(),
      peakSessionCount: this.peakSessionCount,
      totalBytesRelayed: this.totalBytesRelayed,
      totalCheckpointsWritten: this.totalCheckpointsWritten,
      checkpointFailures: this.checkpointFailures,
      clientConnections: this.clientConnections,
      clientDisconnections: this.clientDisconnections,
      respawnCount: this.respawnCount,
      lastClientConnectedAt: this.lastClientConnectedAt,
      memoryUsageBytes: process.memoryUsage().heapUsed,
      fdCount: this.fdCount,
    };
  }

  /** Get the current fd count (last polled value). */
  getFdCount(): number {
    return this.fdCount;
  }

  // -------------------------------------------------------------------------
  // Lifecycle
  // -------------------------------------------------------------------------

  /** Stop the fd polling timer and clean up. */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;

    if (this.fdPollTimer) {
      clearInterval(this.fdPollTimer);
      this.fdPollTimer = null;
    }
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private async pollFdCount(): Promise<void> {
    if (this.disposed) return;
    this.fdCount = await getFdCount();
  }
}
