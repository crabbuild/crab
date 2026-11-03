/**
 * Circuit Breaker — protects against repeated daemon failures.
 *
 * States:
 * - closed: normal operation, requests go to daemon
 * - open: daemon is considered unhealthy, requests route to in-process
 * - half-open: cooldown expired, next attempt decides the state
 *
 * Threshold: 3 failures within a 60-second window → open.
 * Cooldown: 5 minutes in open state → transition to half-open.
 * Half-open: success → closed, failure → open again.
 */

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type CircuitState = "closed" | "open" | "half-open";

export interface CircuitBreakerOptions {
  /** Number of failures within the window to trip the breaker. Default: 3. */
  threshold?: number;
  /** Time window for counting failures (ms). Default: 60_000 (60s). */
  windowMs?: number;
  /** How long the breaker stays open before transitioning to half-open (ms). Default: 300_000 (5 min). */
  cooldownMs?: number;
  /** Logger for state transitions. */
  onStateChange?: (from: CircuitState, to: CircuitState, reason: string) => void;
  /** Clock function for testability. Returns current time in ms. */
  now?: () => number;
}

// ---------------------------------------------------------------------------
// CircuitBreaker
// ---------------------------------------------------------------------------

export class CircuitBreaker {
  private _state: CircuitState = "closed";
  private failures: number[] = [];
  private openedAt: number | null = null;

  private readonly threshold: number;
  private readonly windowMs: number;
  private readonly cooldownMs: number;
  private readonly onStateChange: ((from: CircuitState, to: CircuitState, reason: string) => void) | undefined;
  private readonly now: () => number;

  constructor(opts: CircuitBreakerOptions = {}) {
    this.threshold = opts.threshold ?? 3;
    this.windowMs = opts.windowMs ?? 60_000;
    this.cooldownMs = opts.cooldownMs ?? 300_000;
    this.onStateChange = opts.onStateChange;
    this.now = opts.now ?? (() => Date.now());
  }

  /** Current circuit state. */
  get state(): CircuitState {
    // Check if open state has cooled down → transition to half-open
    if (this._state === "open" && this.openedAt !== null) {
      const elapsed = this.now() - this.openedAt;
      if (elapsed >= this.cooldownMs) {
        this.transition("half-open", "cooldown expired");
      }
    }
    return this._state;
  }

  /**
   * Whether requests should be routed to the daemon.
   * Returns true when the circuit is closed or half-open (attempting recovery).
   */
  get shouldAttemptDaemon(): boolean {
    const current = this.state; // triggers cooldown check
    return current === "closed" || current === "half-open";
  }

  /**
   * Record a failure. If the threshold is reached within the window,
   * the circuit opens.
   */
  recordFailure(): void {
    const timestamp = this.now();

    if (this._state === "half-open") {
      // Half-open failure → back to open
      this.openedAt = timestamp;
      this.failures = [timestamp];
      this.transition("open", "half-open probe failed");
      return;
    }

    if (this._state === "open") {
      // Already open — just record the failure timestamp
      this.failures.push(timestamp);
      return;
    }

    // Closed state — add failure and check threshold
    this.failures.push(timestamp);
    this.pruneOldFailures(timestamp);

    if (this.failures.length >= this.threshold) {
      this.openedAt = timestamp;
      this.transition("open", `${this.failures.length} failures within ${this.windowMs}ms window`);
    }
  }

  /**
   * Record a success. Resets the circuit to closed state if it was
   * half-open. Clears failure history when closing.
   */
  recordSuccess(): void {
    if (this._state === "half-open") {
      this.failures = [];
      this.openedAt = null;
      this.transition("closed", "half-open probe succeeded");
      return;
    }

    if (this._state === "closed") {
      // In closed state, a success doesn't change anything,
      // but we can prune old failures from the window
      this.pruneOldFailures(this.now());
    }
  }

  /**
   * Manually reset the circuit breaker to closed state.
   * Useful for explicit user-triggered daemon restart.
   */
  reset(): void {
    const prev = this._state;
    this.failures = [];
    this.openedAt = null;
    this._state = "closed";
    if (prev !== "closed") {
      this.onStateChange?.(prev, "closed", "manual reset");
    }
  }

  /** Get a snapshot of the current state for diagnostics. */
  getSnapshot(): {
    state: CircuitState;
    failureCount: number;
    openedAt: number | null;
    cooldownRemainingMs: number | null;
  } {
    const current = this.state; // triggers cooldown check
    let cooldownRemainingMs: number | null = null;

    if (current === "open" && this.openedAt !== null) {
      const elapsed = this.now() - this.openedAt;
      cooldownRemainingMs = Math.max(0, this.cooldownMs - elapsed);
    }

    return {
      state: current,
      failureCount: this.failures.length,
      openedAt: this.openedAt,
      cooldownRemainingMs,
    };
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private pruneOldFailures(now: number): void {
    const cutoff = now - this.windowMs;
    this.failures = this.failures.filter((t) => t > cutoff);
  }

  private transition(to: CircuitState, reason: string): void {
    const from = this._state;
    if (from === to) return;
    this._state = to;
    this.onStateChange?.(from, to, reason);
  }
}
