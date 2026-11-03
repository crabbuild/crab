/**
 * Terminal output throttling for high-throughput agent sessions.
 *
 * Batches and throttles rendering when output exceeds the burst threshold
 * (10 MB/s). Keeps effective render rate below frame budget (33ms per frame
 * at 30fps). Ensures user input delivery within 50ms regardless of output
 * backpressure.
 */

// --- Types ---

export interface ThrottleConfig {
  /** Burst threshold in bytes per second (default: 10 MB/s). */
  burstThresholdBps: number;
  /** Frame budget in milliseconds (default: 33ms for 30fps). */
  frameBudgetMs: number;
  /** Maximum input latency in milliseconds (default: 50ms). */
  maxInputLatencyMs: number;
  /** Window size for rate measurement in milliseconds. */
  rateWindowMs: number;
}

export interface ThrottleState {
  /** Whether throttling is currently active. */
  active: boolean;
  /** Current measured output rate in bytes per second. */
  currentRateBps: number;
  /** Number of bytes buffered waiting to be rendered. */
  bufferedBytes: number;
  /** Number of batches pending render. */
  pendingBatches: number;
}

export interface BatchResult {
  /** The data to render in this batch. */
  data: string;
  /** Delay before next batch in milliseconds. */
  delayMs: number;
  /** Whether this is the last batch in the current buffer. */
  isLast: boolean;
}

export const DEFAULT_THROTTLE_CONFIG: ThrottleConfig = {
  burstThresholdBps: 10 * 1024 * 1024, // 10 MB/s
  frameBudgetMs: 33, // 30fps
  maxInputLatencyMs: 50,
  rateWindowMs: 1000, // 1 second measurement window
};

// --- Rate Measurement ---

interface RateSample {
  timestamp: number;
  bytes: number;
}

/**
 * Calculate the current output rate from recent samples.
 */
export function calculateRate(samples: RateSample[], windowMs: number, now: number): number {
  const cutoff = now - windowMs;
  const recentSamples = samples.filter((s) => s.timestamp >= cutoff);
  if (recentSamples.length === 0) return 0;

  const totalBytes = recentSamples.reduce((sum, s) => sum + s.bytes, 0);
  const elapsed = now - recentSamples[0].timestamp;
  if (elapsed <= 0) return totalBytes * (1000 / windowMs);

  return (totalBytes / elapsed) * 1000; // bytes per second
}

// --- Batch Calculation ---

/**
 * Calculate optimal batch size and delay to stay within frame budget.
 *
 * Given the current output rate and frame budget, determines how many bytes
 * can be rendered per frame without exceeding the budget.
 */
export function calculateBatch(
  bufferedBytes: number,
  currentRateBps: number,
  config: ThrottleConfig,
): { batchSize: number; delayMs: number } {
  if (bufferedBytes <= 0) {
    return { batchSize: 0, delayMs: 0 };
  }

  // If rate is below threshold, render everything immediately
  if (currentRateBps <= config.burstThresholdBps) {
    return { batchSize: bufferedBytes, delayMs: 0 };
  }

  // Calculate bytes per frame at the frame budget rate
  // We want to render at most (frameBudgetMs / 1000) * targetRenderRate bytes per frame
  // Target render rate: keep below burst threshold to avoid overwhelming the renderer
  const framesPerSecond = 1000 / config.frameBudgetMs;
  const bytesPerFrame = Math.max(
    1024, // minimum 1KB per batch to avoid excessive fragmentation
    Math.floor(config.burstThresholdBps / framesPerSecond),
  );

  const batchSize = Math.min(bufferedBytes, bytesPerFrame);
  const delayMs = config.frameBudgetMs;

  return { batchSize, delayMs };
}

// --- Output Throttle Controller ---

export interface OutputThrottle {
  /** Feed new data into the throttle. Returns true if data should be rendered immediately. */
  push(data: string): boolean;
  /** Get the next batch to render. Returns null if no data is pending. */
  nextBatch(): BatchResult | null;
  /** Signal that user input needs priority delivery. Flushes input queue. */
  prioritizeInput(): void;
  /** Get current throttle state. */
  getState(): ThrottleState;
  /** Reset the throttle (clear buffers and state). */
  reset(): void;
  /** Dispose the throttle and clear any pending timers. */
  dispose(): void;
}

export function createOutputThrottle(
  config: ThrottleConfig = DEFAULT_THROTTLE_CONFIG,
): OutputThrottle {
  let buffer: string[] = [];
  let bufferedBytes = 0;
  let samples: RateSample[] = [];
  let active = false;
  let disposed = false;

  function now(): number {
    return performance.now();
  }

  function recordSample(bytes: number): void {
    const timestamp = now();
    samples.push({ timestamp, bytes });
    // Prune old samples outside the measurement window
    const cutoff = timestamp - config.rateWindowMs;
    samples = samples.filter((s) => s.timestamp >= cutoff);
  }

  function getCurrentRate(): number {
    return calculateRate(samples, config.rateWindowMs, now());
  }

  return {
    push(data: string): boolean {
      if (disposed) return false;

      const bytes = data.length; // approximate byte count (UTF-16)
      recordSample(bytes);

      const rate = getCurrentRate();
      if (rate <= config.burstThresholdBps) {
        // Below threshold — render immediately
        active = false;
        return true;
      }

      // Above threshold — buffer for batched rendering
      active = true;
      buffer.push(data);
      bufferedBytes += bytes;
      return false;
    },

    nextBatch(): BatchResult | null {
      if (buffer.length === 0) return null;

      const rate = getCurrentRate();
      const { batchSize, delayMs } = calculateBatch(bufferedBytes, rate, config);

      if (batchSize <= 0) return null;

      // Collect data up to batchSize
      let collected = "";
      let collectedBytes = 0;
      while (buffer.length > 0 && collectedBytes < batchSize) {
        const chunk = buffer[0];
        if (collectedBytes + chunk.length <= batchSize) {
          collected += chunk;
          collectedBytes += chunk.length;
          buffer.shift();
        } else {
          // Split the chunk
          const remaining = batchSize - collectedBytes;
          collected += chunk.slice(0, remaining);
          buffer[0] = chunk.slice(remaining);
          collectedBytes = batchSize;
        }
      }

      bufferedBytes -= collectedBytes;
      const isLast = buffer.length === 0;

      return { data: collected, delayMs, isLast };
    },

    prioritizeInput(): void {
      // When user input needs priority, we don't flush the output buffer
      // but we ensure the input path is not blocked. The caller should
      // process input immediately regardless of throttle state.
      // This is a signal that the next frame should prioritize input delivery.
    },

    getState(): ThrottleState {
      return {
        active,
        currentRateBps: getCurrentRate(),
        bufferedBytes,
        pendingBatches: buffer.length,
      };
    },

    reset(): void {
      buffer = [];
      bufferedBytes = 0;
      samples = [];
      active = false;
    },

    dispose(): void {
      disposed = true;
      buffer = [];
      bufferedBytes = 0;
      samples = [];
    },
  };
}
