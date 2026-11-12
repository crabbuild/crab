/**
 * Diagnostics UI IPC — aggregates daemon health data for the
 * Health & Diagnostics panel in the renderer.
 *
 * Registers:
 * - `crab:daemon-diagnostics` IPC handler: returns a unified diagnostics
 *   snapshot combining daemon metrics, circuit breaker state, protocol
 *   version, and daemon mode.
 * - `crab:daemon-restart` IPC handler: triggers a daemon restart and
 *   returns the count of killed sessions.
 *
 * The renderer calls `crab:daemon-diagnostics` on panel open and on a
 * 10s refresh timer. A warning banner is shown when the circuit breaker
 * is open.
 */

import type { DaemonApi } from "./daemon-client";
import type { CircuitBreaker, CircuitState } from "./circuit-breaker";
import type { AutoRespawn } from "./auto-respawn";
import type { IpcMainLike } from "./ipc-bridge";
import type { DaemonMetricsSnapshot } from "./daemon-metrics";
import { PROTOCOL_VERSION } from "./protocol";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface CircuitBreakerSnapshot {
  state: CircuitState;
  failureCount: number;
  openedAt: number | null;
  cooldownRemainingMs: number | null;
}

export interface DiagnosticsSnapshot {
  /** Whether the daemon is running in daemon or in-process mode. */
  mode: "daemon" | "in-process";
  /** Daemon uptime in milliseconds (null if unreachable). */
  uptimeMs: number | null;
  /** Number of currently active PTY sessions (null if unreachable). */
  activeSessionCount: number | null;
  /** Daemon heap memory usage in bytes (null if unreachable). */
  memoryUsageBytes: number | null;
  /** Open file descriptor count (null if unreachable). */
  fdCount: number | null;
  /** Wire protocol version. */
  protocolVersion: number;
  /** Timestamp of last client connection to the daemon (null if never or unreachable). */
  lastClientConnectedAt: number | null;
  /** Circuit breaker state snapshot. */
  circuitBreaker: CircuitBreakerSnapshot;
  /** Total sessions spawned since daemon start (null if unreachable). */
  totalSessionsSpawned: number | null;
  /** Peak concurrent session count (null if unreachable). */
  peakSessionCount: number | null;
  /** Number of daemon respawns (null if unreachable). */
  respawnCount: number | null;
  /** Current memory pressure level derived from daemon heap usage. */
  memoryPressure: "ok" | "warn" | "critical" | null;
  /** Number of sessions currently in high-throughput mode (emulator bypassed). */
  highThroughputSessionCount: number | null;
  /** Whether a path mismatch was detected at startup (stale daemon from a different app location). */
  pathMismatch: boolean;
  /** The entry path recorded in the stale daemon's PID file (null if no mismatch). */
  previousEntryPath: string | null;
  /** Path to the audit log file (null if auditing is disabled or daemon unreachable). */
  auditLogPath: string | null;
  /** Path to the quotas config file (null if daemon unreachable). */
  quotasPath: string | null;
  /** Base path for terminal history (null if daemon unreachable). */
  historyBasePath: string | null;
  /** Breakdown of active sessions by type. */
  resolvedSessionTypeBreakdown: { daemon: number; inProcess: number };
  /**
   * Whether the daemon entry path was resolved from the CRAB_DAEMON_ENTRY
   * environment variable override. Useful for support debugging.
   */
  daemonEntryFromEnv: boolean;
}

export interface RestartResult {
  /** Whether the restart succeeded. */
  success: boolean;
  /** Number of sessions that were killed during restart. */
  killedSessionCount: number;
  /** Error message if restart failed. */
  error?: string;
}

export interface DiagnosticsUiIpcOptions {
  /** Electron ipcMain instance. */
  ipcMain: IpcMainLike;
  /** The DaemonClient instance for querying daemon metrics. */
  client: DaemonApi;
  /** The CircuitBreaker instance for reading breaker state. */
  circuitBreaker: CircuitBreaker;
  /** The AutoRespawn instance for triggering daemon restart. */
  autoRespawn: AutoRespawn;
  /** Returns the current PTY mode for the diagnostics snapshot. */
  getMode: () => "daemon" | "in-process";
  /** Returns the count of active sessions (for restart kill count). */
  getActiveSessionCount?: () => number;
  /** Whether a path mismatch was detected at startup. */
  pathMismatch?: boolean;
  /** The entry path from the stale daemon's PID file (null if no mismatch). */
  previousEntryPath?: string | null;
  /**
   * Returns the current session type breakdown (daemon vs in-process active
   * sessions). Used to populate resolvedSessionTypeBreakdown in the snapshot.
   */
  getSessionTypeBreakdown?: () => { daemon: number; inProcess: number };
  /**
   * Whether the daemon entry path was resolved from the CRAB_DAEMON_ENTRY
   * environment variable override. Surfaced in diagnostics for support debugging.
   */
  daemonEntryFromEnv?: boolean;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/**
 * Register diagnostics UI IPC handlers for the Health & Diagnostics panel.
 *
 * Handlers:
 * - `crab:daemon-diagnostics`: Returns a unified DiagnosticsSnapshot
 *   aggregating daemon metrics, circuit breaker state, and protocol info.
 * - `crab:daemon-restart`: Triggers a daemon restart via AutoRespawn and
 *   returns the count of sessions killed during the restart.
 *
 * Returns a dispose function to remove all handlers.
 */
export function registerDiagnosticsUiIpc(opts: DiagnosticsUiIpcOptions): {
  dispose: () => void;
} {
  const { ipcMain, client, circuitBreaker, autoRespawn, getMode, getActiveSessionCount } = opts;
  const pathMismatch = opts.pathMismatch ?? false;
  const previousEntryPath = opts.previousEntryPath ?? null;
  const daemonEntryFromEnv = opts.daemonEntryFromEnv ?? false;

  // 1-second cache for getDaemonInfo to avoid spamming the daemon on rapid
  // refreshes (the diagnostics panel polls every 10 s, but guard anyway).
  let daemonInfoCache: {
    value: {
      memoryPressure: "ok" | "warn" | "critical";
      highThroughputSessionCount: number;
      auditLogPath: string | null;
      quotasPath: string | null;
      historyBasePath: string | null;
    };
    fetchedAt: number;
  } | null = null;
  const DAEMON_INFO_CACHE_MS = 1_000;

  // -------------------------------------------------------------------------
  // Handler: crab:daemon-diagnostics
  // -------------------------------------------------------------------------

  const handleGetDiagnostics = async (): Promise<DiagnosticsSnapshot> => {
    const mode = getMode();
    const cbSnapshot = circuitBreaker.getSnapshot();

    // Fetch metrics from the daemon (may fail if unreachable)
    let metrics: DaemonMetricsSnapshot | null = null;
    try {
      metrics = await client.request<DaemonMetricsSnapshot>("getMetrics");
    } catch {
      // Daemon unreachable — return what we can from local state
    }

    // Fetch daemon info (cached for 1 s to avoid spamming the daemon).
    type DaemonInfo = {
      memoryPressure: "ok" | "warn" | "critical";
      highThroughputSessionCount: number;
      auditLogPath: string | null;
      quotasPath: string | null;
      historyBasePath: string | null;
    };
    let daemonInfo: DaemonInfo | null = null;
    const now = Date.now();
    if (daemonInfoCache && now - daemonInfoCache.fetchedAt < DAEMON_INFO_CACHE_MS) {
      daemonInfo = daemonInfoCache.value;
    } else {
      try {
        const raw = await client.request<{
          memoryPressure: "ok" | "warn" | "critical";
          highThroughputSessionCount: number;
          auditLogPath: string | null;
          quotasPath: string | null;
          historyBasePath: string | null;
        }>("getDaemonInfo");
        daemonInfo = raw;
        daemonInfoCache = { value: raw, fetchedAt: Date.now() };
      } catch {
        // Daemon unreachable — leave as null
      }
    }

    const breakdown = opts.getSessionTypeBreakdown?.() ?? { daemon: 0, inProcess: 0 };

    return {
      mode,
      uptimeMs: metrics?.uptimeMs ?? null,
      activeSessionCount: metrics?.activeSessionCount ?? null,
      memoryUsageBytes: metrics?.memoryUsageBytes ?? null,
      fdCount: metrics?.fdCount ?? null,
      protocolVersion: PROTOCOL_VERSION,
      lastClientConnectedAt: metrics?.lastClientConnectedAt ?? null,
      circuitBreaker: cbSnapshot,
      totalSessionsSpawned: metrics?.totalSessionsSpawned ?? null,
      peakSessionCount: metrics?.peakSessionCount ?? null,
      respawnCount: metrics?.respawnCount ?? null,
      memoryPressure: daemonInfo?.memoryPressure ?? null,
      highThroughputSessionCount: daemonInfo?.highThroughputSessionCount ?? null,
      pathMismatch,
      previousEntryPath,
      auditLogPath: daemonInfo?.auditLogPath ?? null,
      quotasPath: daemonInfo?.quotasPath ?? null,
      historyBasePath: daemonInfo?.historyBasePath ?? null,
      resolvedSessionTypeBreakdown: breakdown,
      daemonEntryFromEnv,
    };
  };

  // -------------------------------------------------------------------------
  // Handler: crab:daemon-restart
  // -------------------------------------------------------------------------

  const handleRestart = async (): Promise<RestartResult> => {
    // Capture session count before restart (these will be killed)
    const sessionCountBefore = getActiveSessionCount?.() ?? 0;

    try {
      const success = await autoRespawn.restartDaemon();

      if (success) {
        return {
          success: true,
          killedSessionCount: sessionCountBefore,
        };
      }

      return {
        success: false,
        killedSessionCount: sessionCountBefore,
        error: "Restart failed after maximum retry attempts",
      };
    } catch (err) {
      return {
        success: false,
        killedSessionCount: 0,
        error: err instanceof Error ? err.message : String(err),
      };
    }
  };

  // -------------------------------------------------------------------------
  // Register handlers
  // -------------------------------------------------------------------------

  ipcMain.handle("crab:daemon-diagnostics", handleGetDiagnostics);
  ipcMain.handle("crab:daemon-restart", handleRestart);

  // -------------------------------------------------------------------------
  // Dispose
  // -------------------------------------------------------------------------

  function dispose(): void {
    ipcMain.removeHandler("crab:daemon-diagnostics");
    ipcMain.removeHandler("crab:daemon-restart");
  }

  return { dispose };
}
