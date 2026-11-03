import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  ResourceQuotaManager,
  QuotaExceededError,
  type QuotaManagedSession,
  type ResourceQuotaOptions,
} from "../electron/daemon/resource-quotas";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function createMockSession(overrides: Partial<QuotaManagedSession> = {}): QuotaManagedSession {
  return {
    sessionId: overrides.sessionId ?? `session-${Math.random().toString(36).slice(2, 8)}`,
    isAlive: overrides.isAlive ?? true,
    createdAt: overrides.createdAt ?? Date.now(),
    kill: overrides.kill ?? vi.fn(),
  };
}

interface MockConfig {
  files: Map<string, string>;
}

function createMockConfig(): MockConfig {
  return { files: new Map() };
}

function createManager(
  sessions: QuotaManagedSession[],
  mockConfig: MockConfig,
  opts?: Partial<ResourceQuotaOptions>,
): ResourceQuotaManager {
  return new ResourceQuotaManager(() => sessions, {
    configPath: opts?.configPath ?? "/home/user/.crab/desktop/daemon/quotas.json",
    readFile: (filePath: string) => {
      const content = mockConfig.files.get(filePath);
      if (content === undefined) throw new Error("ENOENT");
      return content;
    },
    fileExists: (filePath: string) => mockConfig.files.has(filePath),
    getHeapUsed: opts?.getHeapUsed ?? (() => 100 * 1024 * 1024), // 100 MB default
    onWarning: opts?.onWarning ?? vi.fn(),
    onInfo: opts?.onInfo ?? vi.fn(),
    now: opts?.now ?? (() => Date.now()),
    lifetimeCheckIntervalMs: opts?.lifetimeCheckIntervalMs ?? 60_000,
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("ResourceQuotaManager", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-15T10:00:00.000Z"));
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // -------------------------------------------------------------------------
  // Config loading
  // -------------------------------------------------------------------------

  describe("config loading", () => {
    it("uses defaults when config file does not exist", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig);

      expect(manager.quotas).toEqual({
        maxConcurrentSessions: 50,
        maxMemoryMb: 1024,
        maxSessionLifetimeHours: 0,
      });
    });

    it("loads values from config file", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({
          maxConcurrentSessions: 100,
          maxMemoryMb: 2048,
          maxSessionLifetimeHours: 24,
        }),
      );

      const manager = createManager([], mockConfig);

      expect(manager.quotas).toEqual({
        maxConcurrentSessions: 100,
        maxMemoryMb: 2048,
        maxSessionLifetimeHours: 24,
      });
    });

    it("uses defaults for missing fields in config", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 25 }),
      );

      const manager = createManager([], mockConfig);

      expect(manager.quotas).toEqual({
        maxConcurrentSessions: 25,
        maxMemoryMb: 1024,
        maxSessionLifetimeHours: 0,
      });
    });

    it("uses defaults when config file is malformed JSON", () => {
      const mockConfig = createMockConfig();
      const onWarning = vi.fn();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        "not valid json {{{",
      );

      const manager = createManager([], mockConfig, { onWarning });

      expect(manager.quotas).toEqual({
        maxConcurrentSessions: 50,
        maxMemoryMb: 1024,
        maxSessionLifetimeHours: 0,
      });
      expect(onWarning).toHaveBeenCalledWith(
        "Failed to parse quotas config, using defaults",
        expect.objectContaining({ path: "/home/user/.crab/desktop/daemon/quotas.json" }),
      );
    });

    it("ignores invalid values while allowing zero lifetime to disable killing", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({
          maxConcurrentSessions: -5,
          maxMemoryMb: 0,
          maxSessionLifetimeHours: 0,
        }),
      );

      const manager = createManager([], mockConfig);

      expect(manager.quotas).toEqual({
        maxConcurrentSessions: 50,
        maxMemoryMb: 1024,
        maxSessionLifetimeHours: 0,
      });
    });

    it("uses default lifetime when configured lifetime is invalid", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: -1 }),
      );

      const manager = createManager([], mockConfig);

      expect(manager.quotas.maxSessionLifetimeHours).toBe(0);
    });

    it("floors maxConcurrentSessions to integer", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 25.7 }),
      );

      const manager = createManager([], mockConfig);

      expect(manager.quotas.maxConcurrentSessions).toBe(25);
    });
  });

  // -------------------------------------------------------------------------
  // Session creation checks
  // -------------------------------------------------------------------------

  describe("checkCanCreateSession", () => {
    it("allows creation when under session limit", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig);

      expect(() => manager.checkCanCreateSession(10)).not.toThrow();
    });

    it("throws QuotaExceededError when at session limit", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 5 }),
      );

      const manager = createManager([], mockConfig);

      expect(() => manager.checkCanCreateSession(5)).toThrow(QuotaExceededError);
      expect(() => manager.checkCanCreateSession(5)).toThrow(/maxConcurrentSessions/);
    });

    it("throws QuotaExceededError when memory limit exceeded", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig, {
        getHeapUsed: () => 1100 * 1024 * 1024, // 1100 MB > 1024 MB default
      });

      expect(() => manager.checkCanCreateSession(0)).toThrow(QuotaExceededError);
      expect(() => manager.checkCanCreateSession(0)).toThrow(/maxMemoryMb/);
    });

    it("allows creation when memory is below limit", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig, {
        getHeapUsed: () => 500 * 1024 * 1024, // 500 MB < 1024 MB default
      });

      expect(() => manager.checkCanCreateSession(0)).not.toThrow();
    });

    it("does not throw after dispose", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig, {
        getHeapUsed: () => 2000 * 1024 * 1024, // over limit
      });

      manager.dispose();

      // After dispose, checks are no-ops
      expect(() => manager.checkCanCreateSession(100)).not.toThrow();
    });
  });

  // -------------------------------------------------------------------------
  // Session lifetime enforcement
  // -------------------------------------------------------------------------

  describe("lifetime enforcement", () => {
    it("does not kill old sessions when lifetime quota is disabled by default", () => {
      const mockConfig = createMockConfig();
      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          sessionId: "very-old-session",
          createdAt: now - 30 * 24 * 60 * 60 * 1000,
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });
      manager.enforceLifetime();

      expect(killFn).not.toHaveBeenCalled();
    });

    it("kills sessions that exceed the lifetime quota", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 2 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          sessionId: "old-session",
          createdAt: now - 3 * 60 * 60 * 1000, // 3 hours ago
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });
      manager.enforceLifetime();

      expect(killFn).toHaveBeenCalledTimes(1);
    });

    it("does not kill sessions within the lifetime quota", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 2 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          sessionId: "young-session",
          createdAt: now - 1 * 60 * 60 * 1000, // 1 hour ago
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });
      manager.enforceLifetime();

      expect(killFn).not.toHaveBeenCalled();
    });

    it("skips dead sessions", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 1 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          sessionId: "dead-old-session",
          isAlive: false,
          createdAt: now - 5 * 60 * 60 * 1000, // 5 hours ago
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });
      manager.enforceLifetime();

      expect(killFn).not.toHaveBeenCalled();
    });

    it("kills multiple expired sessions", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 24 }),
      );

      const now = Date.now();
      const kill1 = vi.fn();
      const kill2 = vi.fn();
      const kill3 = vi.fn();
      const sessions = [
        createMockSession({
          sessionId: "expired-1",
          createdAt: now - 25 * 60 * 60 * 1000,
          kill: kill1,
        }),
        createMockSession({
          sessionId: "expired-2",
          createdAt: now - 48 * 60 * 60 * 1000,
          kill: kill2,
        }),
        createMockSession({
          sessionId: "still-valid",
          createdAt: now - 12 * 60 * 60 * 1000,
          kill: kill3,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });
      manager.enforceLifetime();

      expect(kill1).toHaveBeenCalledTimes(1);
      expect(kill2).toHaveBeenCalledTimes(1);
      expect(kill3).not.toHaveBeenCalled();
    });

    it("does not enforce after dispose", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 1 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          createdAt: now - 5 * 60 * 60 * 1000,
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });
      manager.dispose();
      manager.enforceLifetime();

      expect(killFn).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // SIGHUP reload
  // -------------------------------------------------------------------------

  describe("SIGHUP reload", () => {
    it("reloads config on reload() call", () => {
      const mockConfig = createMockConfig();
      const onInfo = vi.fn();
      const manager = createManager([], mockConfig, { onInfo });

      // Initially defaults
      expect(manager.quotas.maxConcurrentSessions).toBe(50);

      // Write new config
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 200 }),
      );

      manager.reload();

      expect(manager.quotas.maxConcurrentSessions).toBe(200);
      expect(onInfo).toHaveBeenCalledWith("Quotas reloaded", expect.objectContaining({
        maxConcurrentSessions: 200,
      }));
    });

    it("falls back to defaults if config becomes malformed on reload", () => {
      const mockConfig = createMockConfig();
      const onWarning = vi.fn();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 100 }),
      );

      const manager = createManager([], mockConfig, { onWarning });
      expect(manager.quotas.maxConcurrentSessions).toBe(100);

      // Corrupt the config
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        "corrupted!!!",
      );

      manager.reload();

      expect(manager.quotas.maxConcurrentSessions).toBe(50); // back to default
      expect(onWarning).toHaveBeenCalledWith(
        "Failed to parse quotas config, using defaults",
        expect.anything(),
      );
    });

    it("falls back to defaults if config file is deleted on reload", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 100 }),
      );

      const manager = createManager([], mockConfig);
      expect(manager.quotas.maxConcurrentSessions).toBe(100);

      // Delete the config
      mockConfig.files.delete("/home/user/.crab/desktop/daemon/quotas.json");

      manager.reload();

      expect(manager.quotas.maxConcurrentSessions).toBe(50); // back to default
    });

    it("does not reload after dispose", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig);

      manager.dispose();

      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 999 }),
      );

      manager.reload();

      // Should still have defaults since reload is a no-op after dispose
      expect(manager.quotas.maxConcurrentSessions).toBe(50);
    });

    it("does not log when config values are unchanged", () => {
      const mockConfig = createMockConfig();
      const onInfo = vi.fn();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({
          maxConcurrentSessions: 50,
          maxMemoryMb: 1024,
          maxSessionLifetimeHours: 0,
        }),
      );

      const manager = createManager([], mockConfig, { onInfo });
      onInfo.mockClear();

      manager.reload();

      expect(onInfo).not.toHaveBeenCalled();
    });
  });

  // -------------------------------------------------------------------------
  // Start / dispose lifecycle
  // -------------------------------------------------------------------------

  describe("start and dispose", () => {
    it("registers SIGHUP handler on start", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig);
      const addListenerSpy = vi.spyOn(process, "on");

      manager.start();

      expect(addListenerSpy).toHaveBeenCalledWith("SIGHUP", expect.any(Function));

      manager.dispose();
      addListenerSpy.mockRestore();
    });

    it("removes SIGHUP handler on dispose", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig);
      const removeListenerSpy = vi.spyOn(process, "removeListener");

      manager.start();
      manager.dispose();

      expect(removeListenerSpy).toHaveBeenCalledWith("SIGHUP", expect.any(Function));

      removeListenerSpy.mockRestore();
    });

    it("starts lifetime enforcement timer on start", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 1 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          createdAt: now - 2 * 60 * 60 * 1000, // 2 hours ago
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, {
        now: () => now,
        lifetimeCheckIntervalMs: 1000,
      });

      manager.start();

      // Timer hasn't fired yet
      expect(killFn).not.toHaveBeenCalled();

      // Advance time to trigger the timer
      vi.advanceTimersByTime(1000);

      expect(killFn).toHaveBeenCalledTimes(1);

      manager.dispose();
    });

    it("stops timer on dispose", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 1 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          createdAt: now - 2 * 60 * 60 * 1000,
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, {
        now: () => now,
        lifetimeCheckIntervalMs: 1000,
      });

      manager.start();
      manager.dispose();

      // Advance time — timer should not fire
      vi.advanceTimersByTime(5000);

      expect(killFn).not.toHaveBeenCalled();
    });

    it("is idempotent — calling start twice does not create duplicate timers", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 1 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          createdAt: now - 2 * 60 * 60 * 1000,
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, {
        now: () => now,
        lifetimeCheckIntervalMs: 1000,
      });

      manager.start();
      manager.start(); // second call should be no-op

      vi.advanceTimersByTime(1000);

      // Should only fire once, not twice
      expect(killFn).toHaveBeenCalledTimes(1);

      manager.dispose();
    });
  });

  // -------------------------------------------------------------------------
  // Quota enforcement with reloaded config
  // -------------------------------------------------------------------------

  describe("enforcement uses reloaded config", () => {
    it("uses updated session limit after reload", () => {
      const mockConfig = createMockConfig();
      const manager = createManager([], mockConfig);

      // Initially allows 50
      expect(() => manager.checkCanCreateSession(49)).not.toThrow();
      expect(() => manager.checkCanCreateSession(50)).toThrow(QuotaExceededError);

      // Reload with higher limit
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxConcurrentSessions: 100 }),
      );
      manager.reload();

      // Now allows up to 100
      expect(() => manager.checkCanCreateSession(50)).not.toThrow();
      expect(() => manager.checkCanCreateSession(99)).not.toThrow();
      expect(() => manager.checkCanCreateSession(100)).toThrow(QuotaExceededError);
    });

    it("uses updated lifetime after reload", () => {
      const mockConfig = createMockConfig();
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 48 }),
      );

      const now = Date.now();
      const killFn = vi.fn();
      const sessions = [
        createMockSession({
          createdAt: now - 30 * 60 * 60 * 1000, // 30 hours ago
          kill: killFn,
        }),
      ];

      const manager = createManager(sessions, mockConfig, { now: () => now });

      // 30h < 48h — should not kill
      manager.enforceLifetime();
      expect(killFn).not.toHaveBeenCalled();

      // Reload with shorter lifetime
      mockConfig.files.set(
        "/home/user/.crab/desktop/daemon/quotas.json",
        JSON.stringify({ maxSessionLifetimeHours: 24 }),
      );
      manager.reload();

      // 30h > 24h — should kill now
      manager.enforceLifetime();
      expect(killFn).toHaveBeenCalledTimes(1);
    });
  });
});
