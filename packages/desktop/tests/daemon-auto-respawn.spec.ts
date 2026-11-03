import { describe, it, expect, vi, beforeEach } from "vitest";
import { AutoRespawn, type AutoRespawnDeps, type AutoRespawnOptions } from "../electron/daemon/auto-respawn";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

interface MockDeps {
  spawner: {
    resetHandle: ReturnType<typeof vi.fn>;
    ensureRunning: ReturnType<typeof vi.fn>;
  };
  getActiveSessionIds: ReturnType<typeof vi.fn>;
  synthesizeExitEvent: ReturnType<typeof vi.fn>;
  reconnect: ReturnType<typeof vi.fn>;
  log: ReturnType<typeof vi.fn>;
}

function createMockDeps(overrides?: Partial<MockDeps>): MockDeps {
  return {
    spawner: {
      resetHandle: vi.fn(),
      ensureRunning: vi.fn().mockResolvedValue(undefined),
      ...overrides?.spawner,
    },
    getActiveSessionIds: overrides?.getActiveSessionIds ?? vi.fn().mockReturnValue([]),
    synthesizeExitEvent: overrides?.synthesizeExitEvent ?? vi.fn(),
    reconnect: overrides?.reconnect ?? vi.fn().mockResolvedValue(undefined),
    log: overrides?.log ?? vi.fn(),
  };
}

function createAutoRespawn(
  depsOverrides?: Partial<MockDeps>,
  opts?: AutoRespawnOptions,
): { respawn: AutoRespawn; deps: MockDeps } {
  const deps = createMockDeps(depsOverrides);
  const respawn = new AutoRespawn(deps as unknown as AutoRespawnDeps, opts);
  return { respawn, deps };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("AutoRespawn", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  // -------------------------------------------------------------------------
  // Basic respawn on disconnect
  // -------------------------------------------------------------------------

  describe("handleDisconnect", () => {
    it("respawns daemon on disconnect", async () => {
      const { respawn, deps } = createAutoRespawn();

      const result = await respawn.handleDisconnect();

      expect(result).toBe(true);
      expect(deps.spawner.resetHandle).toHaveBeenCalledOnce();
      expect(deps.spawner.ensureRunning).toHaveBeenCalledOnce();
      expect(deps.reconnect).toHaveBeenCalledOnce();
      expect(respawn.state).toBe("idle");
      expect(respawn.totalRespawns).toBe(1);
    });

    it("synthesizes exit events for all active sessions before respawn", async () => {
      const { respawn, deps } = createAutoRespawn({
        getActiveSessionIds: vi.fn().mockReturnValue(["session-1", "session-2", "session-3"]),
      });

      await respawn.handleDisconnect();

      expect(deps.synthesizeExitEvent).toHaveBeenCalledTimes(3);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("session-1", -1);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("session-2", -1);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("session-3", -1);
    });

    it("resets spawner handle before attempting respawn", async () => {
      const callOrder: string[] = [];
      const { respawn } = createAutoRespawn({
        spawner: {
          resetHandle: vi.fn().mockImplementation(() => callOrder.push("reset")),
          ensureRunning: vi.fn().mockImplementation(() => {
            callOrder.push("ensureRunning");
            return Promise.resolve();
          }),
        },
        reconnect: vi.fn().mockImplementation(() => {
          callOrder.push("reconnect");
          return Promise.resolve();
        }),
      });

      await respawn.handleDisconnect();

      expect(callOrder).toEqual(["reset", "ensureRunning", "reconnect"]);
    });

    it("returns false when auto-respawn is disabled", async () => {
      const { respawn, deps } = createAutoRespawn(undefined, { enabled: false });

      const result = await respawn.handleDisconnect();

      expect(result).toBe(false);
      expect(deps.spawner.resetHandle).not.toHaveBeenCalled();
      expect(deps.spawner.ensureRunning).not.toHaveBeenCalled();
    });

    it("transitions to failed state after max attempts exhausted", async () => {
      const { respawn } = createAutoRespawn(
        {
          spawner: {
            resetHandle: vi.fn(),
            ensureRunning: vi.fn().mockRejectedValue(new Error("spawn failed")),
          },
        },
        { maxAttempts: 2, retryDelayMs: 10 },
      );

      const promise = respawn.handleDisconnect();
      // Advance timers for retry delays
      await vi.advanceTimersByTimeAsync(100);
      const result = await promise;

      expect(result).toBe(false);
      expect(respawn.state).toBe("failed");
    });

    it("retries on failure up to maxAttempts", async () => {
      const ensureRunning = vi.fn()
        .mockRejectedValueOnce(new Error("first fail"))
        .mockRejectedValueOnce(new Error("second fail"))
        .mockResolvedValueOnce(undefined);

      const { respawn, deps } = createAutoRespawn(
        {
          spawner: {
            resetHandle: vi.fn(),
            ensureRunning,
          },
        },
        { maxAttempts: 3, retryDelayMs: 10 },
      );

      const promise = respawn.handleDisconnect();
      await vi.advanceTimersByTimeAsync(100);
      const result = await promise;

      expect(result).toBe(true);
      expect(ensureRunning).toHaveBeenCalledTimes(3);
      expect(deps.reconnect).toHaveBeenCalledOnce();
    });
  });

  // -------------------------------------------------------------------------
  // Coalescing concurrent respawns
  // -------------------------------------------------------------------------

  describe("coalescing concurrent respawns", () => {
    it("coalesces multiple concurrent handleDisconnect calls", async () => {
      let resolveEnsure: (() => void) | null = null;
      const ensureRunning = vi.fn().mockImplementation(
        () => new Promise<void>((resolve) => { resolveEnsure = resolve; }),
      );

      const { respawn, deps } = createAutoRespawn({
        spawner: { resetHandle: vi.fn(), ensureRunning },
      });

      // Fire multiple disconnect events concurrently
      const p1 = respawn.handleDisconnect();
      const p2 = respawn.handleDisconnect();
      const p3 = respawn.handleDisconnect();

      // All should be the same promise (coalesced)
      expect(respawn.state).toBe("respawning");

      // Resolve the spawner
      resolveEnsure!();
      const [r1, r2, r3] = await Promise.all([p1, p2, p3]);

      expect(r1).toBe(true);
      expect(r2).toBe(true);
      expect(r3).toBe(true);

      // ensureRunning should only be called once (not 3 times)
      expect(ensureRunning).toHaveBeenCalledOnce();
      expect(deps.spawner.resetHandle).toHaveBeenCalledOnce();
    });

    it("allows new respawn after previous completes", async () => {
      const { respawn, deps } = createAutoRespawn();

      await respawn.handleDisconnect();
      await respawn.handleDisconnect();

      // Each call should trigger its own respawn cycle
      expect(deps.spawner.ensureRunning).toHaveBeenCalledTimes(2);
      expect(respawn.totalRespawns).toBe(2);
    });
  });

  // -------------------------------------------------------------------------
  // Retry failed operation after respawn
  // -------------------------------------------------------------------------

  describe("withRetryOnDisconnect", () => {
    it("returns result on first successful attempt", async () => {
      const { respawn } = createAutoRespawn();
      const operation = vi.fn().mockResolvedValue("success");

      const result = await respawn.withRetryOnDisconnect(operation);

      expect(result).toBe("success");
      expect(operation).toHaveBeenCalledOnce();
    });

    it("retries once after disconnect error and respawn", async () => {
      const { respawn } = createAutoRespawn();
      const operation = vi.fn()
        .mockRejectedValueOnce(new Error("Daemon connection lost"))
        .mockResolvedValueOnce("retried-success");

      const result = await respawn.withRetryOnDisconnect(operation);

      expect(result).toBe("retried-success");
      expect(operation).toHaveBeenCalledTimes(2);
    });

    it("retries on 'Control socket not connected' error", async () => {
      const { respawn } = createAutoRespawn();
      const operation = vi.fn()
        .mockRejectedValueOnce(new Error("Control socket not connected"))
        .mockResolvedValueOnce("ok");

      const result = await respawn.withRetryOnDisconnect(operation);

      expect(result).toBe("ok");
      expect(operation).toHaveBeenCalledTimes(2);
    });

    it("retries on 'Client disconnected' error", async () => {
      const { respawn } = createAutoRespawn();
      const operation = vi.fn()
        .mockRejectedValueOnce(new Error("Client disconnected"))
        .mockResolvedValueOnce("recovered");

      const result = await respawn.withRetryOnDisconnect(operation);

      expect(result).toBe("recovered");
    });

    it("does not retry on non-disconnect errors", async () => {
      const { respawn } = createAutoRespawn();
      const operation = vi.fn().mockRejectedValue(new Error("Session not found"));

      await expect(respawn.withRetryOnDisconnect(operation)).rejects.toThrow("Session not found");
      expect(operation).toHaveBeenCalledOnce();
    });

    it("throws original error if respawn fails", async () => {
      const { respawn } = createAutoRespawn(
        {
          spawner: {
            resetHandle: vi.fn(),
            ensureRunning: vi.fn().mockRejectedValue(new Error("spawn failed")),
          },
        },
        { maxAttempts: 1, retryDelayMs: 0 },
      );

      const operation = vi.fn().mockRejectedValue(new Error("Daemon connection lost"));

      await expect(respawn.withRetryOnDisconnect(operation)).rejects.toThrow("Daemon connection lost");
    });

    it("throws if retry also fails after successful respawn", async () => {
      const { respawn } = createAutoRespawn();
      const operation = vi.fn()
        .mockRejectedValueOnce(new Error("Daemon connection lost"))
        .mockRejectedValueOnce(new Error("Still broken"));

      await expect(respawn.withRetryOnDisconnect(operation)).rejects.toThrow("Still broken");
      expect(operation).toHaveBeenCalledTimes(2);
    });
  });

  // -------------------------------------------------------------------------
  // restartDaemon (explicit user-triggered restart)
  // -------------------------------------------------------------------------

  describe("restartDaemon", () => {
    it("restarts daemon successfully", async () => {
      const { respawn, deps } = createAutoRespawn({
        getActiveSessionIds: vi.fn().mockReturnValue(["s1"]),
      });

      const result = await respawn.restartDaemon();

      expect(result).toBe(true);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("s1", -1);
      expect(deps.spawner.resetHandle).toHaveBeenCalledOnce();
      expect(deps.spawner.ensureRunning).toHaveBeenCalledOnce();
      expect(deps.reconnect).toHaveBeenCalledOnce();
      expect(respawn.state).toBe("idle");
    });

    it("coalesces with in-progress respawn", async () => {
      let resolveEnsure: (() => void) | null = null;
      const ensureRunning = vi.fn().mockImplementation(
        () => new Promise<void>((resolve) => { resolveEnsure = resolve; }),
      );

      const { respawn } = createAutoRespawn({
        spawner: { resetHandle: vi.fn(), ensureRunning },
      });

      // Start a disconnect-triggered respawn
      const p1 = respawn.handleDisconnect();
      // Then user triggers explicit restart
      const p2 = respawn.restartDaemon();

      // Both should resolve to the same result
      resolveEnsure!();
      const [r1, r2] = await Promise.all([p1, p2]);

      expect(r1).toBe(true);
      expect(r2).toBe(true);
      expect(ensureRunning).toHaveBeenCalledOnce();
    });

    it("returns false on failure after max attempts", async () => {
      const { respawn } = createAutoRespawn(
        {
          spawner: {
            resetHandle: vi.fn(),
            ensureRunning: vi.fn().mockRejectedValue(new Error("cannot spawn")),
          },
        },
        { maxAttempts: 2, retryDelayMs: 10 },
      );

      const promise = respawn.restartDaemon();
      await vi.advanceTimersByTimeAsync(100);
      const result = await promise;

      expect(result).toBe(false);
      expect(respawn.state).toBe("failed");
    });
  });

  // -------------------------------------------------------------------------
  // Synthesize pty:exit events
  // -------------------------------------------------------------------------

  describe("synthesize exit events", () => {
    it("synthesizes exit with code -1 for all active sessions", async () => {
      const { respawn, deps } = createAutoRespawn({
        getActiveSessionIds: vi.fn().mockReturnValue(["a", "b", "c"]),
      });

      await respawn.handleDisconnect();

      expect(deps.synthesizeExitEvent).toHaveBeenCalledTimes(3);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("a", -1);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("b", -1);
      expect(deps.synthesizeExitEvent).toHaveBeenCalledWith("c", -1);
    });

    it("handles empty session list gracefully", async () => {
      const { respawn, deps } = createAutoRespawn({
        getActiveSessionIds: vi.fn().mockReturnValue([]),
      });

      await respawn.handleDisconnect();

      expect(deps.synthesizeExitEvent).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // State tracking
  // -------------------------------------------------------------------------

  describe("state tracking", () => {
    it("starts in idle state", () => {
      const { respawn } = createAutoRespawn();
      expect(respawn.state).toBe("idle");
      expect(respawn.totalRespawns).toBe(0);
    });

    it("transitions to respawning during respawn", async () => {
      let capturedState: string | null = null;
      const ensureRunning = vi.fn().mockImplementation(() => {
        capturedState = "captured-during-ensure";
        return Promise.resolve();
      });

      const { respawn } = createAutoRespawn({
        spawner: { resetHandle: vi.fn(), ensureRunning },
        reconnect: vi.fn().mockImplementation(() => {
          capturedState = respawn.state;
          return Promise.resolve();
        }),
      });

      await respawn.handleDisconnect();

      expect(capturedState).toBe("respawning");
    });

    it("increments respawn count on each successful respawn", async () => {
      const { respawn } = createAutoRespawn();

      await respawn.handleDisconnect();
      expect(respawn.totalRespawns).toBe(1);

      await respawn.handleDisconnect();
      expect(respawn.totalRespawns).toBe(2);

      await respawn.handleDisconnect();
      expect(respawn.totalRespawns).toBe(3);
    });
  });

  // -------------------------------------------------------------------------
  // Reconnect failure handling
  // -------------------------------------------------------------------------

  describe("reconnect failure", () => {
    it("retries when reconnect fails", async () => {
      const reconnect = vi.fn()
        .mockRejectedValueOnce(new Error("reconnect failed"))
        .mockResolvedValueOnce(undefined);

      const { respawn } = createAutoRespawn(
        { reconnect },
        { maxAttempts: 3, retryDelayMs: 10 },
      );

      const promise = respawn.handleDisconnect();
      await vi.advanceTimersByTimeAsync(100);
      const result = await promise;

      expect(result).toBe(true);
      expect(reconnect).toHaveBeenCalledTimes(2);
    });
  });
});
