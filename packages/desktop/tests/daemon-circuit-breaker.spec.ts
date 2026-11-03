import { describe, it, expect, beforeEach } from "vitest";
import { CircuitBreaker, type CircuitState } from "../electron/daemon/circuit-breaker";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createBreaker(opts?: {
  threshold?: number;
  windowMs?: number;
  cooldownMs?: number;
  now?: () => number;
  onStateChange?: (from: CircuitState, to: CircuitState, reason: string) => void;
}) {
  let clock = 0;
  const nowFn = opts?.now ?? (() => clock);
  const breaker = new CircuitBreaker({
    threshold: opts?.threshold ?? 3,
    windowMs: opts?.windowMs ?? 60_000,
    cooldownMs: opts?.cooldownMs ?? 300_000,
    onStateChange: opts?.onStateChange,
    now: nowFn,
  });
  return { breaker, advance: (ms: number) => { clock += ms; }, setClock: (ms: number) => { clock = ms; } };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("CircuitBreaker", () => {
  // -------------------------------------------------------------------------
  // Opens after threshold
  // -------------------------------------------------------------------------

  describe("opens after threshold", () => {
    it("stays closed below threshold", () => {
      const { breaker } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.state).toBe("closed");
    });

    it("opens after 3 failures within the 60s window", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      advance(10_000);
      breaker.recordFailure();
      advance(10_000);
      breaker.recordFailure();
      expect(breaker.state).toBe("open");
    });

    it("does not open if failures are spread beyond the window", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      advance(30_000);
      breaker.recordFailure();
      advance(31_000); // first failure is now outside the 60s window
      breaker.recordFailure();
      expect(breaker.state).toBe("closed");
    });
  });

  // -------------------------------------------------------------------------
  // Cooldown transitions (open → half-open)
  // -------------------------------------------------------------------------

  describe("cooldown transitions", () => {
    it("remains open before cooldown expires", () => {
      const { breaker, advance } = createBreaker();
      // Trip the breaker
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.state).toBe("open");

      advance(299_999); // just under 5 minutes
      expect(breaker.state).toBe("open");
    });

    it("transitions to half-open after cooldown (5 minutes)", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.state).toBe("open");

      advance(300_000); // exactly 5 minutes
      expect(breaker.state).toBe("half-open");
    });
  });

  // -------------------------------------------------------------------------
  // Success closes (half-open → closed)
  // -------------------------------------------------------------------------

  describe("success closes", () => {
    it("transitions from half-open to closed on success", () => {
      const { breaker, advance } = createBreaker();
      // Trip → open
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      // Cooldown → half-open
      advance(300_000);
      expect(breaker.state).toBe("half-open");

      // Success → closed
      breaker.recordSuccess();
      expect(breaker.state).toBe("closed");
    });

    it("clears failure history on successful close", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      // Access state to trigger half-open transition
      expect(breaker.state).toBe("half-open");
      breaker.recordSuccess();
      expect(breaker.state).toBe("closed");

      // Should need 3 fresh failures to trip again
      advance(1_000);
      breaker.recordFailure();
      advance(1_000);
      breaker.recordFailure();
      expect(breaker.state).toBe("closed");
      advance(1_000);
      breaker.recordFailure();
      expect(breaker.state).toBe("open");
    });
  });

  // -------------------------------------------------------------------------
  // Failure in half-open goes back to open
  // -------------------------------------------------------------------------

  describe("failure in half-open", () => {
    it("transitions from half-open back to open on failure", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      expect(breaker.state).toBe("half-open");

      breaker.recordFailure();
      expect(breaker.state).toBe("open");
    });

    it("requires another full cooldown after half-open failure", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      // Access state to trigger half-open transition
      expect(breaker.state).toBe("half-open");
      breaker.recordFailure(); // back to open
      expect(breaker.state).toBe("open");

      advance(299_999);
      expect(breaker.state).toBe("open");
      advance(1);
      expect(breaker.state).toBe("half-open");
    });
  });

  // -------------------------------------------------------------------------
  // Old failures outside window are pruned
  // -------------------------------------------------------------------------

  describe("failure window pruning", () => {
    it("prunes failures older than the window", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure(); // t=0
      advance(30_000);
      breaker.recordFailure(); // t=30s

      advance(31_000); // t=61s — first failure is now outside window
      breaker.recordFailure(); // t=61s — only 2 failures in window

      expect(breaker.state).toBe("closed");
    });

    it("prunes on success in closed state", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure(); // t=0
      advance(61_000); // past window
      breaker.recordSuccess(); // triggers prune

      // Now add 2 more failures — should not trip (old one pruned)
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.state).toBe("closed");
    });
  });

  // -------------------------------------------------------------------------
  // Manual reset
  // -------------------------------------------------------------------------

  describe("manual reset", () => {
    it("resets from open to closed", () => {
      const { breaker } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.state).toBe("open");

      breaker.reset();
      expect(breaker.state).toBe("closed");
    });

    it("resets from half-open to closed", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      expect(breaker.state).toBe("half-open");

      breaker.reset();
      expect(breaker.state).toBe("closed");
    });

    it("clears failure history on reset", () => {
      const { breaker } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.reset();

      // Should need 3 fresh failures to trip
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.state).toBe("closed");
    });

    it("reset on already-closed is a no-op", () => {
      const transitions: Array<{ from: CircuitState; to: CircuitState }> = [];
      const { breaker } = createBreaker({
        onStateChange: (from, to) => transitions.push({ from, to }),
      });

      breaker.reset();
      expect(transitions).toHaveLength(0);
    });
  });

  // -------------------------------------------------------------------------
  // shouldAttemptDaemon
  // -------------------------------------------------------------------------

  describe("shouldAttemptDaemon", () => {
    it("returns true when closed", () => {
      const { breaker } = createBreaker();
      expect(breaker.shouldAttemptDaemon).toBe(true);
    });

    it("returns false when open", () => {
      const { breaker } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      expect(breaker.shouldAttemptDaemon).toBe(false);
    });

    it("returns true when half-open (attempting recovery)", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      expect(breaker.shouldAttemptDaemon).toBe(true);
    });
  });

  // -------------------------------------------------------------------------
  // State change callback
  // -------------------------------------------------------------------------

  describe("state change callback", () => {
    it("invoked on closed → open transition", () => {
      const transitions: Array<{ from: CircuitState; to: CircuitState; reason: string }> = [];
      const { breaker } = createBreaker({
        onStateChange: (from, to, reason) => transitions.push({ from, to, reason }),
      });

      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();

      expect(transitions).toHaveLength(1);
      expect(transitions[0].from).toBe("closed");
      expect(transitions[0].to).toBe("open");
      expect(transitions[0].reason).toContain("3 failures");
    });

    it("invoked on open → half-open transition", () => {
      const transitions: Array<{ from: CircuitState; to: CircuitState; reason: string }> = [];
      const { breaker, advance } = createBreaker({
        onStateChange: (from, to, reason) => transitions.push({ from, to, reason }),
      });

      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      // Access state to trigger cooldown check
      void breaker.state;

      expect(transitions).toHaveLength(2);
      expect(transitions[1].from).toBe("open");
      expect(transitions[1].to).toBe("half-open");
      expect(transitions[1].reason).toContain("cooldown");
    });

    it("invoked on half-open → closed transition", () => {
      const transitions: Array<{ from: CircuitState; to: CircuitState; reason: string }> = [];
      const { breaker, advance } = createBreaker({
        onStateChange: (from, to, reason) => transitions.push({ from, to, reason }),
      });

      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      void breaker.state; // trigger half-open
      breaker.recordSuccess();

      expect(transitions).toHaveLength(3);
      expect(transitions[2].from).toBe("half-open");
      expect(transitions[2].to).toBe("closed");
      expect(transitions[2].reason).toContain("succeeded");
    });

    it("invoked on half-open → open transition (probe failed)", () => {
      const transitions: Array<{ from: CircuitState; to: CircuitState; reason: string }> = [];
      const { breaker, advance } = createBreaker({
        onStateChange: (from, to, reason) => transitions.push({ from, to, reason }),
      });

      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(300_000);
      void breaker.state; // trigger half-open
      breaker.recordFailure();

      expect(transitions).toHaveLength(3);
      expect(transitions[2].from).toBe("half-open");
      expect(transitions[2].to).toBe("open");
      expect(transitions[2].reason).toContain("probe failed");
    });

    it("invoked on manual reset", () => {
      const transitions: Array<{ from: CircuitState; to: CircuitState; reason: string }> = [];
      const { breaker } = createBreaker({
        onStateChange: (from, to, reason) => transitions.push({ from, to, reason }),
      });

      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.reset();

      expect(transitions).toHaveLength(2);
      expect(transitions[1].from).toBe("open");
      expect(transitions[1].to).toBe("closed");
      expect(transitions[1].reason).toContain("manual reset");
    });
  });

  // -------------------------------------------------------------------------
  // getSnapshot diagnostics
  // -------------------------------------------------------------------------

  describe("getSnapshot", () => {
    it("returns correct snapshot in closed state", () => {
      const { breaker } = createBreaker();
      const snap = breaker.getSnapshot();
      expect(snap.state).toBe("closed");
      expect(snap.failureCount).toBe(0);
      expect(snap.openedAt).toBeNull();
      expect(snap.cooldownRemainingMs).toBeNull();
    });

    it("returns correct snapshot in open state with cooldown remaining", () => {
      const { breaker, advance } = createBreaker();
      breaker.recordFailure();
      breaker.recordFailure();
      breaker.recordFailure();
      advance(100_000); // 100s into cooldown

      const snap = breaker.getSnapshot();
      expect(snap.state).toBe("open");
      expect(snap.failureCount).toBe(3);
      expect(snap.openedAt).toBe(0); // opened at t=0
      expect(snap.cooldownRemainingMs).toBe(200_000); // 300k - 100k
    });
  });
});
