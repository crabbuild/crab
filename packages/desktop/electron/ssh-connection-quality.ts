/**
 * Connection quality monitoring with bandwidth estimation and packet loss detection.
 *
 * Extends the latency measurement from SshLatencyMonitor with bandwidth probes
 * and packet loss tracking. Emits quality level transitions (green/yellow/red)
 * based on configurable thresholds.
 *
 * Quality levels:
 * - green: latency <100ms AND packet loss <5%
 * - yellow: latency 100–200ms OR packet loss 5–15%
 * - red: latency >200ms OR packet loss >15%
 *
 * Measurement interval: 10 seconds. A missing response within 10s is treated
 * as 100% packet loss for that interval.
 */

import { EventEmitter } from "node:events";
import type { SshLatencyMonitor } from "./ssh-latency-monitor";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { ConnectionQuality,QualityLevel } from "./ssh-types";

// ── Constants ────────────────────────────────────────────────────

/** Interval between quality measurements (milliseconds). */
const MEASUREMENT_INTERVAL_MS = 10_000;

/** Timeout for a single measurement probe (milliseconds). */
const PROBE_TIMEOUT_MS = 10_000;

/** Size of the bandwidth probe payload in bytes (small probe). */
const BANDWIDTH_PROBE_BYTES = 4096;

/** Number of recent measurements to retain for averaging. */
const HISTORY_SIZE = 6;

// ── Quality Thresholds ───────────────────────────────────────────

const GREEN_LATENCY_MAX_MS = 100;
const YELLOW_LATENCY_MAX_MS = 200;
const GREEN_LOSS_MAX_PERCENT = 5;
const YELLOW_LOSS_MAX_PERCENT = 15;

// ── Types ────────────────────────────────────────────────────────

interface MeasurementSample {
  latencyMs: number;
  packetLoss: boolean;
  bandwidthMbps: number;
  timestamp: number;
}

export interface QualityChangedEvent {
  profileId: string;
  previous: QualityLevel;
  current: QualityLevel;
  quality: ConnectionQuality;
}

// ── ConnectionQualityMonitor ─────────────────────────────────────

export class ConnectionQualityMonitor extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  private readonly measurementIntervalMs: number;

  /** Active measurement timers keyed by profileId. */
  private timers = new Map<string, NodeJS.Timeout>();

  /** Current quality state per profile. */
  private qualityState = new Map<string, ConnectionQuality>();

  /** Recent measurement history per profile (sliding window). */
  private history = new Map<string, MeasurementSample[]>();

  constructor(
    _latencyMonitor: SshLatencyMonitor,
    router: RpcRouter,
    sessionManager: SshSessionManager,
    measurementIntervalMs: number = MEASUREMENT_INTERVAL_MS,
  ) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
    this.measurementIntervalMs = measurementIntervalMs;
  }

  /**
   * Get the current connection quality for a profile, or null if
   * no measurement has been taken yet.
   */
  getQuality(profileId: string): ConnectionQuality | null {
    return this.qualityState.get(profileId) ?? null;
  }

  /**
   * Start quality monitoring for a profile. Performs an immediate
   * measurement, then repeats at the configured interval (default 10s).
   *
   * No-op if already monitoring this profile.
   */
  startMonitoring(profileId: string): void {
    if (this.timers.has(profileId)) {
      return;
    }

    // Initialize history
    this.history.set(profileId, []);

    // Perform an immediate measurement
    void this.measure(profileId);

    // Schedule periodic measurements
    const timer = setInterval(() => {
      void this.measure(profileId);
    }, this.measurementIntervalMs);

    this.timers.set(profileId, timer);
  }

  /**
   * Stop quality monitoring for a profile. Clears the timer and
   * removes cached quality state.
   */
  stopMonitoring(profileId: string): void {
    const timer = this.timers.get(profileId);
    if (timer) {
      clearInterval(timer);
      this.timers.delete(profileId);
    }
    this.qualityState.delete(profileId);
    this.history.delete(profileId);
  }

  /**
   * Stop monitoring all profiles.
   */
  stopAll(): void {
    for (const [, timer] of this.timers) {
      clearInterval(timer);
    }
    this.timers.clear();
    this.qualityState.clear();
    this.history.clear();
  }

  /**
   * Get the set of profile IDs currently being monitored.
   */
  getMonitoredProfiles(): string[] {
    return Array.from(this.timers.keys());
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Perform a single quality measurement cycle: latency probe,
   * bandwidth estimation, and packet loss detection.
   */
  private async measure(profileId: string): Promise<void> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      return;
    }

    const sample = await this.probe(profileId);
    this.recordSample(profileId, sample);
    this.updateQuality(profileId);
  }

  /**
   * Send a probe to the remote agent and measure latency + bandwidth.
   * If the probe times out (>10s), treat as 100% packet loss.
   */
  private async probe(profileId: string): Promise<MeasurementSample> {
    const startMs = Date.now();

    try {
      const result = await this.withTimeout(
        this.router.sendToRemoteAgent(profileId, "bandwidth_probe", {
          payloadSize: BANDWIDTH_PROBE_BYTES,
        }),
        PROBE_TIMEOUT_MS,
      );

      const elapsedMs = Date.now() - startMs;
      const responseBytes = (result as any)?.responseSize ?? BANDWIDTH_PROBE_BYTES;

      // Bandwidth = bytes transferred / time in seconds, converted to Mbps
      const transferredBytes = BANDWIDTH_PROBE_BYTES + responseBytes;
      const elapsedSec = elapsedMs / 1000;
      const bandwidthMbps = elapsedSec > 0
        ? (transferredBytes * 8) / (elapsedSec * 1_000_000)
        : 0;

      return {
        latencyMs: elapsedMs,
        packetLoss: false,
        bandwidthMbps,
        timestamp: Date.now(),
      };
    } catch {
      // Timeout or error — treat as packet loss
      return {
        latencyMs: PROBE_TIMEOUT_MS,
        packetLoss: true,
        bandwidthMbps: 0,
        timestamp: Date.now(),
      };
    }
  }

  /**
   * Record a measurement sample in the sliding window history.
   */
  private recordSample(profileId: string, sample: MeasurementSample): void {
    const samples = this.history.get(profileId) ?? [];
    samples.push(sample);

    // Keep only the most recent HISTORY_SIZE samples
    if (samples.length > HISTORY_SIZE) {
      samples.shift();
    }

    this.history.set(profileId, samples);
  }

  /**
   * Compute aggregate quality metrics from the history and update
   * the quality state. Emits "quality-changed" if the level transitions.
   */
  private updateQuality(profileId: string): void {
    const samples = this.history.get(profileId);
    if (!samples || samples.length === 0) {
      return;
    }

    // Compute averages from the sliding window
    const avgLatencyMs = samples.reduce((sum, s) => sum + s.latencyMs, 0) / samples.length;
    const packetLossPercent = (samples.filter((s) => s.packetLoss).length / samples.length) * 100;
    const avgBandwidthMbps =
      samples.filter((s) => !s.packetLoss).reduce((sum, s) => sum + s.bandwidthMbps, 0) /
      Math.max(samples.filter((s) => !s.packetLoss).length, 1);

    const newLevel = this.computeLevel(avgLatencyMs, packetLossPercent);
    const previousQuality = this.qualityState.get(profileId);
    const previousLevel = previousQuality?.level ?? null;

    const quality: ConnectionQuality = {
      level: newLevel,
      latencyMs: Math.round(avgLatencyMs),
      bandwidthMbps: Math.round(avgBandwidthMbps * 100) / 100,
      packetLossPercent: Math.round(packetLossPercent * 100) / 100,
      measuredAt: Date.now(),
    };

    this.qualityState.set(profileId, quality);

    // Emit event on level transition
    if (previousLevel !== null && previousLevel !== newLevel) {
      const event: QualityChangedEvent = {
        profileId,
        previous: previousLevel,
        current: newLevel,
        quality,
      };
      this.emit("quality-changed", event);
    }
  }

  /**
   * Determine the quality level based on latency and packet loss thresholds.
   *
   * - green: latency <100ms AND loss <5%
   * - red: latency >200ms OR loss >15%
   * - yellow: everything in between
   */
  private computeLevel(latencyMs: number, packetLossPercent: number): QualityLevel {
    if (latencyMs > YELLOW_LATENCY_MAX_MS || packetLossPercent > YELLOW_LOSS_MAX_PERCENT) {
      return "red";
    }
    if (latencyMs < GREEN_LATENCY_MAX_MS && packetLossPercent < GREEN_LOSS_MAX_PERCENT) {
      return "green";
    }
    return "yellow";
  }

  /**
   * Wrap a promise with a timeout. Rejects if the promise doesn't
   * resolve within the specified duration.
   */
  private withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`Probe timed out after ${timeoutMs}ms`));
      }, timeoutMs);

      promise.then(
        (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        (err) => {
          clearTimeout(timer);
          reject(err);
        },
      );
    });
  }
}
