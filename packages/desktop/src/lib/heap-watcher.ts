/**
 * Renderer heap watcher — samples `performance.memory.usedJSHeapSize`
 * (Chromium-only, which is fine since we ship Electron) every 5 seconds.
 * When usage exceeds the configurable cap, fires a callback so the
 * eviction manager can offload the least-recently-focused tab's state
 * to OPFS.
 *
 * Gracefully no-ops when `performance.memory` is unavailable (e.g.
 * non-Chromium test runners).
 */

export interface HeapWatcherOptions {
  /** Cap in megabytes. Default: 1024 (1 GB). */
  capMb: number;
  /** Sampling interval in milliseconds. Default: 5000 (5 s). */
  intervalMs?: number;
  /** Called when heap usage exceeds the cap. */
  onOverCap?: (usageBytes: number) => void;
}

/** Chromium-specific `performance.memory` shape. */
interface PerformanceMemory {
  usedJSHeapSize: number;
  totalJSHeapSize: number;
  jsHeapSizeLimit: number;
}

function getMemory(): PerformanceMemory | null {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const mem = (performance as any).memory as PerformanceMemory | undefined;
  if (mem && typeof mem.usedJSHeapSize === "number") return mem;
  return null;
}

const DEFAULT_INTERVAL_MS = 5_000;

export class HeapWatcher {
  private timer: ReturnType<typeof setInterval> | null = null;
  private _capMb: number;
  private _intervalMs: number;
  private _onOverCap: ((usageBytes: number) => void) | null;
  private _lastUsageBytes = 0;
  private _available: boolean;

  constructor(opts: HeapWatcherOptions) {
    this._capMb = opts.capMb;
    this._intervalMs = opts.intervalMs ?? DEFAULT_INTERVAL_MS;
    this._onOverCap = opts.onOverCap ?? null;
    this._available = getMemory() !== null;
  }

  /** Whether `performance.memory` is available in this environment. */
  get available(): boolean {
    return this._available;
  }

  /** Last sampled heap usage in bytes. */
  get usageBytes(): number {
    return this._lastUsageBytes;
  }

  /** Last sampled heap usage in megabytes (rounded to 1 decimal). */
  get usageMb(): number {
    return Math.round((this._lastUsageBytes / (1024 * 1024)) * 10) / 10;
  }

  /** Current cap in MB. */
  get capMb(): number {
    return this._capMb;
  }

  /** Update the cap at runtime (e.g. when the user changes settings). */
  setCapMb(capMb: number): void {
    this._capMb = capMb;
  }

  /** Start sampling. No-op if already running or if the API is unavailable. */
  start(): void {
    if (this.timer || !this._available) return;
    this.timer = setInterval(() => this.sample(), this._intervalMs);
    // Take an immediate sample so consumers don't wait 5 s for the first reading.
    this.sample();
  }

  /** Stop sampling. */
  stop(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Take a single sample (exposed for testing / manual trigger). */
  sample(): void {
    const mem = getMemory();
    if (!mem) return;
    this._lastUsageBytes = mem.usedJSHeapSize;
    const capBytes = this._capMb * 1024 * 1024;
    if (this._lastUsageBytes > capBytes && this._onOverCap) {
      this._onOverCap(this._lastUsageBytes);
    }
  }

  /** Clean up. Call on unmount / app exit. */
  destroy(): void {
    this.stop();
    this._onOverCap = null;
  }
}
