/**
 * Remote System Health Monitor — polls remote system metrics (disk,
 * memory, GPU) and emits health updates for the UI.
 *
 * Polls every 30s while the SSH view or Dashboard is visible. Stops
 * polling when the view is hidden to save bandwidth.
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export interface DiskInfo {
  path: string;
  total: number;
  free: number;
  usedPercent: number;
}

export interface MemoryInfo {
  total: number;
  free: number;
  cached: number;
  usedPercent: number;
}

export interface GpuInfo {
  index: number;
  name: string;
  utilization: number;
  memoryUsed: number;
  memoryTotal: number;
  temperature: number;
  processes: Array<{ pid: number; name: string; memUsed: number }>;
}

export interface SystemHealth {
  os: string;
  kernel: string;
  arch: string;
  hostname: string;
  uptime: number;
  loadAvg: [number, number, number];
  disk: DiskInfo[];
  memory: MemoryInfo;
  gpus?: GpuInfo[];
  collectedAt: number;
}

export interface HealthAlert {
  type: "disk" | "memory" | "gpu_temp";
  severity: "warning" | "critical";
  message: string;
  value: number;
  threshold: number;
}

export interface SystemHealthMonitorEvents {
  "ssh:health-update": (payload: { profileId: string; health: SystemHealth }) => void;
  "health:alert": (payload: { profileId: string; alert: HealthAlert }) => void;
  "health:alerts-snapshot": (payload: { profileId: string; alerts: HealthAlert[] }) => void;
  "health:error": (payload: { profileId: string; error: string }) => void;
}

// ── Constants ────────────────────────────────────────────────────

const DEFAULT_POLL_INTERVAL_MS = 30_000;
const DISK_WARNING_THRESHOLD = 90;
const MEMORY_WARNING_THRESHOLD = 90;
const GPU_TEMP_WARNING_THRESHOLD = 85;

/**
 * Deduplication key for health alerts. Combines the alert type with
 * the profileId so the same alert isn't emitted repeatedly on every
 * poll cycle. Only emits when an alert is NEW (wasn't present in the
 * previous poll for that profile).
 */
function alertDedupKey(profileId: string, alert: HealthAlert): string {
  return `${profileId}:${alert.type}:${alert.message}`;
}

// ── System Health Monitor ────────────────────────────────────────

/**
 * Polls remote system metrics at a configurable interval and emits
 * `ssh:health-update` events when new data arrives.
 *
 * Supports starting/stopping polling per profile. Only polls when
 * the SSH view or Dashboard is visible (controlled by `setVisible()`).
 * Caches the last health result per profile.
 */
export class SystemHealthMonitor extends EventEmitter {
  private router: RpcRouter;
  private pollIntervals = new Map<string, ReturnType<typeof setInterval>>();
  private lastHealth = new Map<string, SystemHealth>();
  private pollIntervalMs: number;
  private visible = false;
  private activeProfiles = new Set<string>();
  /** Tracks active alert keys per profile for deduplication. */
  private activeAlerts = new Map<string, Set<string>>();

  constructor(
    router: RpcRouter,
    _sessionManager: SshSessionManager,
    pollIntervalMs?: number,
  ) {
    super();
    this.router = router;
    this.pollIntervalMs = pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS;
  }

  /**
   * Control whether the health monitor should actively poll.
   * When the SSH view or Dashboard is not visible, polling is paused
   * to save bandwidth. When it becomes visible again, polling resumes
   * for all active profiles.
   */
  setVisible(visible: boolean): void {
    const wasVisible = this.visible;
    this.visible = visible;

    if (visible && !wasVisible) {
      // Resume polling for all active profiles
      for (const profileId of this.activeProfiles) {
        this.startInterval(profileId);
      }
    } else if (!visible && wasVisible) {
      // Pause all polling intervals (keep activeProfiles set intact)
      for (const [, interval] of this.pollIntervals) {
        clearInterval(interval);
      }
      this.pollIntervals.clear();
    }
  }

  /**
   * Whether the monitor is currently in visible (active polling) state.
   */
  isVisible(): boolean {
    return this.visible;
  }

  /**
   * Start polling health metrics for a connected profile.
   * Does an immediate fetch, then polls at the configured interval.
   * If the view is not visible, the profile is registered but polling
   * is deferred until `setVisible(true)` is called.
   */
  startPolling(profileId: string): void {
    this.activeProfiles.add(profileId);

    if (!this.visible) {
      // Defer actual polling until the view becomes visible
      return;
    }

    this.startInterval(profileId);
  }

  /**
   * Stop polling for a profile (e.g., on disconnect).
   * Removes the profile from the active set entirely.
   */
  stopPolling(profileId: string): void {
    this.activeProfiles.delete(profileId);
    this.activeAlerts.delete(profileId);

    const interval = this.pollIntervals.get(profileId);
    if (interval) {
      clearInterval(interval);
      this.pollIntervals.delete(profileId);
    }
  }

  /**
   * Stop all polling (e.g., on app quit).
   */
  stopAll(): void {
    for (const [, interval] of this.pollIntervals) {
      clearInterval(interval);
    }
    this.pollIntervals.clear();
    this.activeProfiles.clear();
    this.activeAlerts.clear();
  }

  /**
   * Get the last collected health data for a profile (cached).
   */
  getLastHealth(profileId: string): SystemHealth | undefined {
    return this.lastHealth.get(profileId);
  }

  /**
   * Force an immediate health check (e.g., user clicked refresh).
   */
  async refresh(profileId: string): Promise<SystemHealth> {
    return this.pollHealth(profileId);
  }

  /**
   * Returns the set of profiles currently registered for polling.
   */
  getActiveProfiles(): Set<string> {
    return new Set(this.activeProfiles);
  }

  /**
   * Returns whether a profile currently has an active polling interval.
   */
  isPolling(profileId: string): boolean {
    return this.pollIntervals.has(profileId);
  }

  /**
   * Returns the current set of active alert dedup keys for a profile.
   * Useful for testing and diagnostics.
   */
  getActiveAlertKeys(profileId: string): Set<string> {
    return new Set(this.activeAlerts.get(profileId) ?? []);
  }

  /**
   * Generate a one-liner summary for the connection card.
   * e.g., "4x A100 - 87% util - 1.8TB free - 96GB/128GB RAM"
   */
  formatSummary(health: SystemHealth): string {
    const parts: string[] = [];

    // GPU summary
    if (health.gpus && health.gpus.length > 0) {
      const gpuCount = health.gpus.length;
      const avgUtil = Math.round(
        health.gpus.reduce((sum, g) => sum + g.utilization, 0) / gpuCount,
      );
      const gpuName = health.gpus[0].name.replace(/NVIDIA\s*/i, "").trim();
      parts.push(`${gpuCount}x ${gpuName} - ${avgUtil}% util`);
    }

    // Disk summary (largest mount)
    if (health.disk.length > 0) {
      const mainDisk = health.disk.reduce((a, b) => (a.total > b.total ? a : b));
      parts.push(`${this.formatBytes(mainDisk.free)} free`);
    }

    // Memory summary
    const memUsed = health.memory.total - health.memory.free;
    parts.push(
      `${this.formatBytes(memUsed)}/${this.formatBytes(health.memory.total)} RAM`,
    );

    return parts.join(" - ");
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Start the actual polling interval for a profile. Performs an
   * immediate poll, then sets up the recurring interval.
   */
  private startInterval(profileId: string): void {
    if (this.pollIntervals.has(profileId)) return;

    // Immediate first poll (fire-and-forget; errors are emitted as events)
    this.pollHealth(profileId).catch(() => {});

    const interval = setInterval(() => {
      this.pollHealth(profileId).catch(() => {});
    }, this.pollIntervalMs);

    this.pollIntervals.set(profileId, interval);
  }

  private async pollHealth(profileId: string): Promise<SystemHealth> {
    const repoId = `ssh://${profileId}/`;

    try {
      const result = await this.router.route(repoId, "system_health", {
        include_gpu: true,
      }) as SystemHealth;

      result.collectedAt = Date.now();
      this.lastHealth.set(profileId, result);
      this.emit("ssh:health-update", { profileId, health: result });

      // Check for alerts
      this.checkAlerts(profileId, result);

      return result;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("health:error", { profileId, error: message });

      // Return last known health if available
      const last = this.lastHealth.get(profileId);
      if (last) return last;

      throw err;
    }
  }

  private checkAlerts(profileId: string, health: SystemHealth): void {
    const currentAlerts = new Set<string>();
    const newAlerts: HealthAlert[] = [];
    const previousAlerts = this.activeAlerts.get(profileId) ?? new Set();

    // Disk alerts
    for (const disk of health.disk) {
      if (disk.usedPercent >= DISK_WARNING_THRESHOLD) {
        const alert: HealthAlert = {
          type: "disk",
          severity: disk.usedPercent >= 95 ? "critical" : "warning",
          message: `Disk ${disk.path} is ${disk.usedPercent}% full`,
          value: disk.usedPercent,
          threshold: DISK_WARNING_THRESHOLD,
        };
        const key = alertDedupKey(profileId, alert);
        currentAlerts.add(key);
        if (!previousAlerts.has(key)) {
          newAlerts.push(alert);
        }
      }
    }

    // Memory alerts
    if (health.memory.usedPercent >= MEMORY_WARNING_THRESHOLD) {
      const alert: HealthAlert = {
        type: "memory",
        severity: health.memory.usedPercent >= 95 ? "critical" : "warning",
        message: `Memory usage at ${health.memory.usedPercent}%`,
        value: health.memory.usedPercent,
        threshold: MEMORY_WARNING_THRESHOLD,
      };
      const key = alertDedupKey(profileId, alert);
      currentAlerts.add(key);
      if (!previousAlerts.has(key)) {
        newAlerts.push(alert);
      }
    }

    // GPU temperature alerts
    if (health.gpus) {
      for (const gpu of health.gpus) {
        if (gpu.temperature >= GPU_TEMP_WARNING_THRESHOLD) {
          const alert: HealthAlert = {
            type: "gpu_temp",
            severity: gpu.temperature >= 90 ? "critical" : "warning",
            message: `GPU ${gpu.index} (${gpu.name}) at ${gpu.temperature}°C`,
            value: gpu.temperature,
            threshold: GPU_TEMP_WARNING_THRESHOLD,
          };
          const key = alertDedupKey(profileId, alert);
          currentAlerts.add(key);
          if (!previousAlerts.has(key)) {
            newAlerts.push(alert);
          }
        }
      }
    }

    // Update the active alerts set for this profile
    this.activeAlerts.set(profileId, currentAlerts);

    // Emit only NEW alerts (deduplication: don't spam on every poll)
    for (const alert of newAlerts) {
      this.emit("health:alert", { profileId, alert });
    }

    // Always emit the full set of current alerts for state tracking
    const allAlerts = newAlerts.length > 0 || currentAlerts.size !== previousAlerts.size
      ? this.collectCurrentAlerts(health)
      : undefined;
    if (allAlerts !== undefined) {
      this.emit("health:alerts-snapshot", { profileId, alerts: allAlerts });
    }
  }

  /**
   * Collect all current alerts from a health snapshot (no dedup logic,
   * just the raw alert list for state tracking).
   */
  private collectCurrentAlerts(health: SystemHealth): HealthAlert[] {
    const alerts: HealthAlert[] = [];

    for (const disk of health.disk) {
      if (disk.usedPercent >= DISK_WARNING_THRESHOLD) {
        alerts.push({
          type: "disk",
          severity: disk.usedPercent >= 95 ? "critical" : "warning",
          message: `Disk ${disk.path} is ${disk.usedPercent}% full`,
          value: disk.usedPercent,
          threshold: DISK_WARNING_THRESHOLD,
        });
      }
    }

    if (health.memory.usedPercent >= MEMORY_WARNING_THRESHOLD) {
      alerts.push({
        type: "memory",
        severity: health.memory.usedPercent >= 95 ? "critical" : "warning",
        message: `Memory usage at ${health.memory.usedPercent}%`,
        value: health.memory.usedPercent,
        threshold: MEMORY_WARNING_THRESHOLD,
      });
    }

    if (health.gpus) {
      for (const gpu of health.gpus) {
        if (gpu.temperature >= GPU_TEMP_WARNING_THRESHOLD) {
          alerts.push({
            type: "gpu_temp",
            severity: gpu.temperature >= 90 ? "critical" : "warning",
            message: `GPU ${gpu.index} (${gpu.name}) at ${gpu.temperature}°C`,
            value: gpu.temperature,
            threshold: GPU_TEMP_WARNING_THRESHOLD,
          });
        }
      }
    }

    return alerts;
  }

  private formatBytes(bytes: number): string {
    if (bytes >= 1_099_511_627_776) {
      return `${(bytes / 1_099_511_627_776).toFixed(1)}TB`;
    }
    if (bytes >= 1_073_741_824) {
      return `${(bytes / 1_073_741_824).toFixed(1)}GB`;
    }
    if (bytes >= 1_048_576) {
      return `${(bytes / 1_048_576).toFixed(0)}MB`;
    }
    return `${(bytes / 1024).toFixed(0)}KB`;
  }
}
