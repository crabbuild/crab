/**
 * Local JSONL telemetry buffer with HTTPS flush and exponential backoff.
 *
 * Events are appended to an in-memory buffer and flushed every 5 minutes
 * to `https://telemetry.crab.dev/v1/events`. The flush is a stub in
 * dev (console.log only) since the endpoint doesn't exist yet.
 *
 * Only active when `telemetryOptIn` is true AND `--no-telemetry` was
 * not passed as a launch flag.
 */

export interface TelemetryEvent {
  /** ISO 8601 timestamp. */
  ts: string;
  /** Event name, e.g. "repo.open", "commit.create". */
  event: string;
  /** Arbitrary structured payload. No PII. */
  data: Record<string, unknown>;
}

const FLUSH_INTERVAL_MS = 5 * 60 * 1000; // 5 minutes
const MAX_BUFFER_SIZE = 500;
const ENDPOINT = "https://telemetry.crab.dev/v1/events";
const MAX_BACKOFF_MS = 5 * 60 * 1000; // 5 minutes max backoff

export class TelemetryBuffer {
  private buffer: TelemetryEvent[] = [];
  private flushTimer: ReturnType<typeof setInterval> | null = null;
  private backoffMs = 1000;
  private _enabled = false;
  private _noTelemetryFlag = false;

  get enabled(): boolean {
    return this._enabled && !this._noTelemetryFlag;
  }

  /** Set the --no-telemetry flag state. When true, telemetry is
   *  disabled regardless of the opt-in setting. */
  setNoTelemetryFlag(flag: boolean): void {
    this._noTelemetryFlag = flag;
    if (flag) this.stop();
  }

  /** Enable or disable based on the user's opt-in setting. */
  setOptIn(optIn: boolean): void {
    this._enabled = optIn;
    if (this.enabled) {
      this.start();
    } else {
      this.stop();
    }
  }

  /** Append an event to the buffer. No-op if disabled. */
  track(event: string, data: Record<string, unknown> = {}): void {
    if (!this.enabled) return;
    const entry: TelemetryEvent = {
      ts: new Date().toISOString(),
      event,
      data,
    };
    this.buffer.push(entry);
    if (this.buffer.length > MAX_BUFFER_SIZE) {
      this.buffer.shift();
    }
  }

  /** Get a copy of the current buffer for preview purposes. */
  getBuffer(): TelemetryEvent[] {
    return [...this.buffer];
  }

  /** Get the buffer as JSONL string for preview. */
  getBufferAsJsonl(): string {
    return this.buffer.map((e) => JSON.stringify(e)).join("\n");
  }

  /** Flush the buffer to the telemetry endpoint. */
  async flush(): Promise<void> {
    if (!this.enabled || this.buffer.length === 0) return;

    const batch = [...this.buffer];
    const payload = batch.map((e) => JSON.stringify(e)).join("\n");

    try {
      // In development, just log to console. The endpoint doesn't
      // exist yet, so we avoid actual HTTP calls.
      if (
        typeof window !== "undefined" &&
        window.location &&
        (window.location.hostname === "localhost" ||
          window.location.protocol === "file:")
      ) {
        console.debug("[telemetry] flush (dev stub):", batch.length, "events");
        this.buffer = [];
        this.backoffMs = 1000;
        return;
      }

      const res = await fetch(ENDPOINT, {
        method: "POST",
        headers: { "Content-Type": "application/x-ndjson" },
        body: payload,
      });

      if (res.ok) {
        // Success: clear the flushed events and reset backoff.
        this.buffer = [];
        this.backoffMs = 1000;
      } else {
        // Server error: apply backoff.
        this.applyBackoff();
      }
    } catch {
      // Network error: apply backoff.
      this.applyBackoff();
    }
  }

  private applyBackoff(): void {
    this.backoffMs = Math.min(this.backoffMs * 2, MAX_BACKOFF_MS);
  }

  private start(): void {
    if (this.flushTimer) return;
    this.flushTimer = setInterval(() => {
      void this.flush();
    }, FLUSH_INTERVAL_MS);
  }

  private stop(): void {
    if (this.flushTimer) {
      clearInterval(this.flushTimer);
      this.flushTimer = null;
    }
  }

  /** Shut down the buffer. Call on app exit. */
  destroy(): void {
    this.stop();
    this.buffer = [];
  }
}

/** Singleton instance used across the app. */
export const telemetry = new TelemetryBuffer();
