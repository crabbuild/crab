/**
 * Token-bucket bandwidth throttler shared across all SFTP transfer streams.
 *
 * Interactive SSH channels (terminals, RPC calls) are exempt from throttling.
 * The throttler refills tokens at 100ms intervals and enforces a configurable
 * bytes-per-second limit. When the limit is 0, no throttling is applied.
 *
 * Integrates with SshLatencyMonitor to accept latency updates for adaptive
 * throttling (implemented in task 3.2).
 */

import { EventEmitter } from "node:events";
import type { SshLatencyMonitor } from "./ssh-latency-monitor";

// ── Constants ────────────────────────────────────────────────────

/** Interval at which tokens are refilled (milliseconds). */
const TOKEN_REFILL_INTERVAL_MS = 100;

/** Sliding window duration for throughput measurement (milliseconds). */
const THROUGHPUT_WINDOW_MS = 5_000;


/** Latency threshold above which adaptive throttling activates (ms). */
const ADAPTIVE_HIGH_LATENCY_MS = 200;

/** Latency threshold below which recovery counting begins (ms). */
const ADAPTIVE_LOW_LATENCY_MS = 150;

/** Number of consecutive low-latency readings required to restore bandwidth. */
const ADAPTIVE_RECOVERY_COUNT = 10;

// ── Types ────────────────────────────────────────────────────────

export interface ThrottlerConfig {
  maxBytesPerSecond: number;
  adaptiveEnabled: boolean;
  adaptiveFloor: number;
}

interface ThroughputSample {
  timestamp: number;
  bytes: number;
}

interface PendingConsumer {
  bytes: number;
  resolve: () => void;
}

// ── BandwidthThrottler ───────────────────────────────────────────

export class BandwidthThrottler extends EventEmitter {

  /** User-configured limit in bytes per second. 0 means unlimited. */
  private limitBytesPerSec: number = 0;

  /** Effective limit after adaptive throttling adjustments. */
  private effectiveLimitBytesPerSec: number = 0;

  /** Available tokens (bytes) in the bucket. */
  private tokensAvailable: number = 0;

  /** Maximum bucket capacity (tokens refill up to this). */
  private bucketCapacity: number = 0;

  /** Timer for periodic token refill. */
  private refillTimer: NodeJS.Timeout | null = null;

  /** Timestamp of the last token refill. */
  private lastRefillAt: number = 0;

  /** Queue of pending consumers waiting for tokens. */
  private pendingQueue: PendingConsumer[] = [];

  /** Sliding window of throughput samples for getCurrentThroughput(). */
  private throughputSamples: ThroughputSample[] = [];

  /** Whether adaptive throttling is enabled. */
  private adaptiveEnabled: boolean = false;

  /** Minimum floor for adaptive throttling (bytes/sec). */
  private adaptiveFloor: number = 10 * 1024; // 10 KB/s

  /** Whether adaptive reduction is currently active. */
  private adaptiveReductionActive: boolean = false;

  /** Count of consecutive low-latency readings (< 150ms) for recovery. */
  private consecutiveLowLatencyCount: number = 0;

  constructor(_latencyMonitor: SshLatencyMonitor, config?: Partial<ThrottlerConfig>) {
    super();

    if (config) {
      this.limitBytesPerSec = config.maxBytesPerSecond ?? 0;
      this.adaptiveEnabled = config.adaptiveEnabled ?? false;
      this.adaptiveFloor = config.adaptiveFloor ?? 10 * 1024;
    }

    this.effectiveLimitBytesPerSec = this.limitBytesPerSec;
    this.recalculateBucket();
    this.startRefillTimer();
  }

  /**
   * Request tokens for a transfer chunk. Resolves when tokens are available.
   * If the limit is 0 (unlimited), resolves immediately.
   */
  async consumeTokens(bytes: number): Promise<void> {
    // Record throughput regardless of throttling
    this.recordThroughput(bytes);

    // No throttling when limit is 0
    if (this.effectiveLimitBytesPerSec === 0) {
      return;
    }

    // If enough tokens are available, consume immediately
    if (this.tokensAvailable >= bytes) {
      this.tokensAvailable -= bytes;
      return;
    }

    // Otherwise, queue the consumer and wait for tokens
    return new Promise<void>((resolve) => {
      this.pendingQueue.push({ bytes, resolve });
    });
  }

  /**
   * Update the bandwidth limit. Applies within 2 seconds.
   * A value of 0 means unlimited (no throttling).
   */
  setLimit(bytesPerSecond: number): void {
    this.limitBytesPerSec = Math.max(0, bytesPerSecond);

    if (this.adaptiveReductionActive) {
      // Adaptive throttling is active — recalculate effective limit
      this.applyAdaptiveReduction();
    } else {
      this.effectiveLimitBytesPerSec = this.limitBytesPerSec;
    }

    this.recalculateBucket();

    // Flush pending queue if limit is now unlimited
    if (this.effectiveLimitBytesPerSec === 0) {
      this.flushAllPending();
    }
  }

  /**
   * Get the user-configured limit in bytes per second.
   */
  getLimit(): number {
    return this.limitBytesPerSec;
  }

  /**
   * Get the effective limit after adaptive adjustments.
   */
  getEffectiveLimit(): number {
    return this.effectiveLimitBytesPerSec;
  }

  /**
   * Get current aggregate throughput measured over a sliding window (bytes/sec).
   */
  getCurrentThroughput(): number {
    const now = Date.now();
    this.pruneThroughputSamples(now);

    if (this.throughputSamples.length === 0) {
      return 0;
    }

    const windowStart = now - THROUGHPUT_WINDOW_MS;
    let totalBytes = 0;
    for (const sample of this.throughputSamples) {
      if (sample.timestamp >= windowStart) {
        totalBytes += sample.bytes;
      }
    }

    // Calculate the actual window duration (from first sample to now)
    const firstSampleTime = this.throughputSamples[0]?.timestamp ?? now;
    const windowDuration = Math.max(now - firstSampleTime, 1000); // At least 1s to avoid spikes
    const effectiveWindow = Math.min(windowDuration, THROUGHPUT_WINDOW_MS);

    return (totalBytes / effectiveWindow) * 1000;
  }

  /**
   * Whether adaptive throttling is currently reducing bandwidth.
   */
  isAdaptiveReductionActive(): boolean {
    return this.adaptiveReductionActive;
  }

  /**
   * Enable or disable adaptive throttling.
   */
  setAdaptiveEnabled(enabled: boolean): void {
    this.adaptiveEnabled = enabled;
    if (!enabled && this.adaptiveReductionActive) {
      this.adaptiveReductionActive = false;
      this.effectiveLimitBytesPerSec = this.limitBytesPerSec;
      this.recalculateBucket();
    }
  }

  /**
   * Handler for latency updates from the SshLatencyMonitor.
   * Implements the adaptive throttling state machine:
   * - Latency > 200ms: activate reduction (50% of configured limit, min 10 KB/s)
   * - Latency < 150ms for 10 consecutive readings: deactivate reduction
   * - Any reading >= 150ms resets the recovery counter
   */
  onLatencyUpdate(profileId: string, latencyMs: number): void {
    if (!this.adaptiveEnabled || this.limitBytesPerSec === 0) {
      return;
    }

    this.emit("latency-update", { profileId, latencyMs });

    if (latencyMs > ADAPTIVE_HIGH_LATENCY_MS) {
      // High latency detected — activate reduction and reset recovery counter
      this.consecutiveLowLatencyCount = 0;
      if (!this.adaptiveReductionActive) {
        this.activateAdaptiveReduction();
      }
    } else if (latencyMs < ADAPTIVE_LOW_LATENCY_MS) {
      // Low latency — increment recovery counter
      this.consecutiveLowLatencyCount++;
      if (this.adaptiveReductionActive && this.consecutiveLowLatencyCount >= ADAPTIVE_RECOVERY_COUNT) {
        this.deactivateAdaptiveReduction();
        this.consecutiveLowLatencyCount = 0;
      }
    } else {
      // Latency in the dead zone [150, 200] — reset recovery counter but don't activate
      this.consecutiveLowLatencyCount = 0;
    }
  }

  /**
   * Apply adaptive reduction (reduce to 50% of configured limit).
   * Called when latency exceeds threshold.
   */
  activateAdaptiveReduction(): void {
    if (!this.adaptiveEnabled || this.limitBytesPerSec === 0) {
      return;
    }
    this.adaptiveReductionActive = true;
    this.applyAdaptiveReduction();
    this.recalculateBucket();
  }

  /**
   * Deactivate adaptive reduction, restoring the user-configured limit.
   */
  deactivateAdaptiveReduction(): void {
    this.adaptiveReductionActive = false;
    this.effectiveLimitBytesPerSec = this.limitBytesPerSec;
    this.recalculateBucket();
  }

  /**
   * Stop the throttler and release all pending consumers.
   */
  destroy(): void {
    if (this.refillTimer) {
      clearInterval(this.refillTimer);
      this.refillTimer = null;
    }
    this.flushAllPending();
    this.throughputSamples = [];
    this.removeAllListeners();
  }

  // ── Private ────────────────────────────────────────────────────

  private applyAdaptiveReduction(): void {
    const reduced = Math.floor(this.limitBytesPerSec * 0.5);
    this.effectiveLimitBytesPerSec = Math.max(reduced, this.adaptiveFloor);
  }

  /**
   * Recalculate bucket capacity based on the effective limit.
   * Bucket holds one full second of data to allow reasonable burst
   * while still enforcing the rate limit over time.
   */
  private recalculateBucket(): void {
    if (this.effectiveLimitBytesPerSec === 0) {
      this.bucketCapacity = 0;
      this.tokensAvailable = 0;
      return;
    }

    // Bucket capacity = 1 second worth of data (allows burst up to 1s)
    this.bucketCapacity = this.effectiveLimitBytesPerSec;

    // Cap current tokens to new capacity
    this.tokensAvailable = Math.min(this.tokensAvailable, this.bucketCapacity);
  }

  /**
   * Start the periodic token refill timer.
   */
  private startRefillTimer(): void {
    this.lastRefillAt = Date.now();

    this.refillTimer = setInterval(() => {
      this.refillTokens();
    }, TOKEN_REFILL_INTERVAL_MS);

    // Ensure the timer doesn't prevent process exit
    if (this.refillTimer.unref) {
      this.refillTimer.unref();
    }
  }

  /**
   * Refill tokens based on elapsed time and the effective limit.
   * Then attempt to drain the pending queue.
   */
  private refillTokens(): void {
    if (this.effectiveLimitBytesPerSec === 0) {
      // Unlimited — flush any pending consumers that might have been
      // queued before the limit was removed
      this.flushAllPending();
      return;
    }

    const now = Date.now();
    const elapsed = now - this.lastRefillAt;
    this.lastRefillAt = now;

    // Calculate tokens to add based on actual elapsed time
    const tokensToAdd = (this.effectiveLimitBytesPerSec * elapsed) / 1000;
    this.tokensAvailable = Math.min(
      this.tokensAvailable + tokensToAdd,
      this.bucketCapacity,
    );

    // Drain pending queue
    this.drainPendingQueue();
  }

  /**
   * Attempt to satisfy pending consumers from available tokens.
   * For large requests that exceed bucket capacity, we partially
   * consume tokens each refill until the full request is satisfied.
   */
  private drainPendingQueue(): void {
    while (this.pendingQueue.length > 0) {
      const next = this.pendingQueue[0];

      if (next.bytes <= this.tokensAvailable) {
        // Enough tokens to satisfy the full request
        this.tokensAvailable -= next.bytes;
        this.pendingQueue.shift();
        next.resolve();
      } else if (next.bytes > this.bucketCapacity && this.tokensAvailable > 0) {
        // Request exceeds bucket capacity — consume what we can and reduce remaining
        next.bytes -= this.tokensAvailable;
        this.tokensAvailable = 0;
        break;
      } else {
        // Not enough tokens yet — wait for more refills
        break;
      }
    }
  }

  /**
   * Flush all pending consumers immediately (used when limit becomes unlimited or on destroy).
   */
  private flushAllPending(): void {
    const pending = this.pendingQueue.splice(0);
    for (const consumer of pending) {
      consumer.resolve();
    }
  }

  /**
   * Record a throughput sample for the sliding window.
   */
  private recordThroughput(bytes: number): void {
    const now = Date.now();
    this.throughputSamples.push({ timestamp: now, bytes });
    this.pruneThroughputSamples(now);
  }

  /**
   * Remove throughput samples older than the sliding window.
   */
  private pruneThroughputSamples(now: number): void {
    const cutoff = now - THROUGHPUT_WINDOW_MS;
    while (this.throughputSamples.length > 0 && this.throughputSamples[0].timestamp < cutoff) {
      this.throughputSamples.shift();
    }
  }
}
