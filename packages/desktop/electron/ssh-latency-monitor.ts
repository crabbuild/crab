/**
 * Connection health monitoring via periodic latency measurement.
 *
 * Sends a `ping` RPC to each connected remote agent every 20 seconds
 * and measures the round-trip time. Emits `ssh:latency-update` events
 * to the renderer and feeds latency data into the RemoteCache for
 * adaptive TTL computation.
 *
 * Also detects high-latency mode (>200ms RTT) and emits
 * `ssh:high-latency-mode` events. High-latency mode is activated
 * immediately when latency exceeds 200ms, and deactivated only after
 * 3 consecutive measurements below 200ms (hysteresis to avoid flapping).
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteCache } from "./ssh-cache";

// ── Types ────────────────────────────────────────────────────────

export interface LatencyMonitorEvents {
  "ssh:latency-update": (payload: { profileId: string; latencyMs: number }) => void;
  "ssh:high-latency-mode": (payload: { profileId: string; enabled: boolean }) => void;
}

// ── Constants ────────────────────────────────────────────────────

/** Default interval between latency measurements (milliseconds). */
const DEFAULT_PING_INTERVAL_MS = 20_000;

/** Latency threshold for high-latency mode activation (milliseconds). */
const HIGH_LATENCY_THRESHOLD_MS = 200;

/** Number of consecutive measurements below threshold required to deactivate high-latency mode. */
const HIGH_LATENCY_DEACTIVATION_COUNT = 3;

// ── Latency Monitor ──────────────────────────────────────────────

/**
 * Periodically measures connection latency by sending a `ping` RPC
 * to each connected remote agent and timing the round-trip.
 *
 * Results are:
 * - Emitted as `ssh:latency-update` events for the renderer
 * - Fed into the RemoteCache via `updateLatency()` for adaptive TTL
 *
 * High-latency mode:
 * - Activated immediately when RTT exceeds 200ms
 * - Deactivated after 3 consecutive measurements below 200ms
 * - Emitted as `ssh:high-latency-mode` events for the renderer
 *
 * The monitor only pings profiles that are currently in "connected"
 * status. It automatically starts/stops pinging as connections come
 * and go.
 */
export class SshLatencyMonitor extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  private readonly cache: RemoteCache | null;
  private readonly pingIntervalMs: number;

  /** Active polling timers keyed by profileId. */
  private timers = new Map<string, NodeJS.Timeout>();

  /** Last measured latency per profile (for deduplication). */
  private lastLatency = new Map<string, number>();

  /** Whether high-latency mode is currently active per profile. */
  private highLatencyActive = new Map<string, boolean>();

  /** Count of consecutive low-latency measurements per profile (for hysteresis). */
  private lowLatencyStreak = new Map<string, number>();

  constructor(
    router: RpcRouter,
    sessionManager: SshSessionManager,
    cache: RemoteCache | null = null,
    pingIntervalMs: number = DEFAULT_PING_INTERVAL_MS,
  ) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
    this.cache = cache;
    this.pingIntervalMs = pingIntervalMs;
  }

  /**
   * Start periodic latency measurement for a profile.
   * Performs an immediate measurement, then repeats at the configured interval.
   *
   * No-op if already monitoring this profile.
   */
  startMonitoring(profileId: string): void {
    if (this.timers.has(profileId)) {
      return;
    }

    // Perform an immediate measurement
    void this.measureLatency(profileId);

    // Schedule periodic measurements
    const timer = setInterval(() => {
      void this.measureLatency(profileId);
    }, this.pingIntervalMs);

    this.timers.set(profileId, timer);
  }

  /**
   * Stop monitoring a specific profile. Clears the timer and removes
   * cached latency data.
   */
  stopMonitoring(profileId: string): void {
    const timer = this.timers.get(profileId);
    if (timer) {
      clearInterval(timer);
      this.timers.delete(profileId);
    }
    this.lastLatency.delete(profileId);
    this.highLatencyActive.delete(profileId);
    this.lowLatencyStreak.delete(profileId);
  }

  /**
   * Stop monitoring all profiles. Call on app shutdown or when the
   * SSH subsystem is torn down.
   */
  stopAll(): void {
    for (const [, timer] of this.timers) {
      clearInterval(timer);
    }
    this.timers.clear();
    this.lastLatency.clear();
    this.highLatencyActive.clear();
    this.lowLatencyStreak.clear();
  }

  /**
   * Get the set of profile IDs currently being monitored.
   */
  getMonitoredProfiles(): string[] {
    return Array.from(this.timers.keys());
  }

  /**
   * Get the last measured latency for a profile, or undefined if
   * no measurement has been taken yet.
   */
  getLastLatency(profileId: string): number | undefined {
    return this.lastLatency.get(profileId);
  }

  /**
   * Check whether high-latency mode is currently active for a profile.
   */
  isHighLatencyMode(profileId: string): boolean {
    return this.highLatencyActive.get(profileId) ?? false;
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Measure the round-trip time of a `ping` RPC to the remote agent.
   * On success, emits the latency and updates the cache. On failure
   * (timeout, disconnected), silently skips — the connection lifecycle
   * handles error states separately.
   */
  private async measureLatency(profileId: string): Promise<void> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      return;
    }

    const startMs = Date.now();

    try {
      await this.router.sendToRemoteAgent(profileId, "ping", {});
      const latencyMs = Date.now() - startMs;

      this.lastLatency.set(profileId, latencyMs);

      // Feed into cache for adaptive TTL
      if (this.cache) {
        this.cache.updateLatency(profileId, latencyMs);
      }

      // Evaluate high-latency mode
      this.evaluateHighLatencyMode(profileId, latencyMs);

      // Emit to renderer
      this.emit("ssh:latency-update", { profileId, latencyMs });
    } catch {
      // Ping failed — connection may be degraded or reconnecting.
      // Don't emit an update; the connection status events handle
      // surfacing errors to the user.
    }
  }

  /**
   * Evaluate whether high-latency mode should be activated or deactivated
   * based on the latest measurement.
   *
   * Activation: immediate when latency exceeds the threshold.
   * Deactivation: requires 3 consecutive measurements below the threshold
   * to avoid flapping on transient latency spikes.
   */
  private evaluateHighLatencyMode(profileId: string, latencyMs: number): void {
    const currentlyActive = this.highLatencyActive.get(profileId) ?? false;

    if (latencyMs > HIGH_LATENCY_THRESHOLD_MS) {
      // Reset the low-latency streak counter
      this.lowLatencyStreak.set(profileId, 0);

      if (!currentlyActive) {
        // Activate high-latency mode immediately
        this.highLatencyActive.set(profileId, true);
        this.emit("ssh:high-latency-mode", { profileId, enabled: true });
      }
    } else {
      // Latency is below threshold
      if (currentlyActive) {
        // Increment the low-latency streak
        const streak = (this.lowLatencyStreak.get(profileId) ?? 0) + 1;
        this.lowLatencyStreak.set(profileId, streak);

        if (streak >= HIGH_LATENCY_DEACTIVATION_COUNT) {
          // Deactivate high-latency mode after sustained low latency
          this.highLatencyActive.set(profileId, false);
          this.lowLatencyStreak.set(profileId, 0);
          this.emit("ssh:high-latency-mode", { profileId, enabled: false });
        }
      } else {
        // Not active and latency is low — reset streak counter
        this.lowLatencyStreak.set(profileId, 0);
      }
    }
  }
}
