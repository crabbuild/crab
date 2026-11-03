/**
 * StreamDataBatcher — timer-based output coalescing for PTY data.
 *
 * Consecutive data chunks for the same session are concatenated in the
 * batch queue. The batch flushes on timer expiry (8ms) or when explicitly
 * flushed (before exit events, to ensure final output isn't stranded).
 *
 * This matches the main-process PTY IPC batching cadence and avoids
 * adding latency while reducing socket write syscalls during bursty
 * output (e.g., compilation logs).
 */

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Default batch interval in milliseconds. */
const DEFAULT_BATCH_INTERVAL_MS = 8;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface BatchEntry {
  sessionId: string;
  data: Buffer;
}

export type FlushCallback = (clientId: string, entries: BatchEntry[]) => void;

export interface StreamDataBatcherOptions {
  /** Batch interval in ms (default: 8). */
  intervalMs?: number;
  /** Called when a batch is flushed. */
  onFlush: FlushCallback;
}

// ---------------------------------------------------------------------------
// StreamDataBatcher
// ---------------------------------------------------------------------------

export class StreamDataBatcher {
  private intervalMs: number;
  private onFlush: FlushCallback;
  private queues: Map<string, BatchEntry[]> = new Map();
  private timers: Map<string, ReturnType<typeof setTimeout>> = new Map();
  private disposed = false;

  constructor(opts: StreamDataBatcherOptions) {
    this.intervalMs = opts.intervalMs ?? DEFAULT_BATCH_INTERVAL_MS;
    this.onFlush = opts.onFlush;
  }

  /**
   * Enqueue data for a client+session. Starts the batch timer if not
   * already running for this client.
   */
  enqueue(clientId: string, sessionId: string, data: Buffer): void {
    if (this.disposed) return;

    let queue = this.queues.get(clientId);
    if (!queue) {
      queue = [];
      this.queues.set(clientId, queue);
    }

    // Coalesce: if the last entry is for the same session, concatenate
    const last = queue[queue.length - 1];
    if (last && last.sessionId === sessionId) {
      last.data = Buffer.concat([last.data, data]);
    } else {
      queue.push({ sessionId, data: Buffer.from(data) });
    }

    // Start timer if not already running for this client
    if (!this.timers.has(clientId)) {
      const timer = setTimeout(() => {
        this.timers.delete(clientId);
        this.flushClient(clientId);
      }, this.intervalMs);
      this.timers.set(clientId, timer);
    }
  }

  /**
   * Explicitly flush all pending data for a specific client.
   * Used before exit events to ensure final output isn't stranded.
   */
  flush(clientId: string): void {
    // Cancel pending timer
    const timer = this.timers.get(clientId);
    if (timer) {
      clearTimeout(timer);
      this.timers.delete(clientId);
    }
    this.flushClient(clientId);
  }

  /**
   * Flush all clients. Used during shutdown.
   */
  flushAll(): void {
    for (const clientId of [...this.queues.keys()]) {
      this.flush(clientId);
    }
  }

  /**
   * Clear pending data for a specific client (on disconnect).
   * If no clientId is provided, clears all clients.
   */
  clear(clientId?: string): void {
    if (clientId) {
      const timer = this.timers.get(clientId);
      if (timer) {
        clearTimeout(timer);
        this.timers.delete(clientId);
      }
      this.queues.delete(clientId);
    } else {
      for (const timer of this.timers.values()) {
        clearTimeout(timer);
      }
      this.timers.clear();
      this.queues.clear();
    }
  }

  /**
   * Dispose the batcher — clear all timers and queues.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.clear();
  }

  /**
   * Number of clients with pending data.
   */
  get pendingClients(): number {
    return this.queues.size;
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private flushClient(clientId: string): void {
    const queue = this.queues.get(clientId);
    if (!queue || queue.length === 0) {
      this.queues.delete(clientId);
      return;
    }

    // Take the queue and clear it
    this.queues.delete(clientId);

    // Invoke callback
    this.onFlush(clientId, queue);
  }
}
