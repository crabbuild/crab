import { describe, it, expect, vi, beforeEach } from "vitest";

import {
  parsePtyMode,
  initializePtyMode,
  registerPtyModeIpc,
  type PtyModeConfig,
  type ModeSelectionDeps,
  type ModeSelectionResult,
} from "../electron/daemon/mode-selection";

import type { IpcMainLike } from "../electron/daemon/ipc-bridge";

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

function createMockIpcMain(): IpcMainLike & {
  handlers: Map<string, (...args: unknown[]) => unknown>;
  invoke: (channel: string, ...args: unknown[]) => Promise<unknown>;
} {
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  return {
    handlers,
    handle(channel: string, listener: (...args: unknown[]) => unknown) {
      handlers.set(channel, listener);
    },
    removeHandler(channel: string) {
      handlers.delete(channel);
    },
    async invoke(channel: string, ...args: unknown[]) {
      const handler = handlers.get(channel);
      if (!handler) throw new Error(`No handler for ${channel}`);
      return handler(null, ...args);
    },
  };
}

function createMockSpawner(opts: { shouldFail?: boolean; error?: string } = {}) {
  return {
    ensureRunning: opts.shouldFail
      ? vi.fn().mockRejectedValue(new Error(opts.error ?? "Spawn failed"))
      : vi.fn().mockResolvedValue(undefined),
  };
}

function createMockClient(opts: { shouldFail?: boolean; error?: string } = {}) {
  return {
    ensureConnected: opts.shouldFail
      ? vi.fn().mockRejectedValue(new Error(opts.error ?? "Connection failed"))
      : vi.fn().mockResolvedValue(undefined),
  };
}

function createMockDeps(opts: {
  spawnerFails?: boolean;
  clientFails?: boolean;
  spawnerError?: string;
  clientError?: string;
} = {}): ModeSelectionDeps & { ipcMain: ReturnType<typeof createMockIpcMain> } {
  const ipcMain = createMockIpcMain();
  const spawner = createMockSpawner({
    shouldFail: opts.spawnerFails,
    error: opts.spawnerError,
  });
  const client = createMockClient({
    shouldFail: opts.clientFails,
    error: opts.clientError,
  });
  const log = vi.fn();

  return {
    client: client as unknown as ModeSelectionDeps["client"],
    spawner: spawner as unknown as ModeSelectionDeps["spawner"],
    ipcMain,
    log,
  };
}

// ---------------------------------------------------------------------------
// parsePtyMode
// ---------------------------------------------------------------------------

describe("parsePtyMode", () => {
  it("returns 'auto' when env var is undefined", () => {
    expect(parsePtyMode(undefined)).toBe("auto");
  });

  it("returns 'auto' when env var is empty string", () => {
    expect(parsePtyMode("")).toBe("auto");
  });

  it("returns 'daemon' for 'daemon'", () => {
    expect(parsePtyMode("daemon")).toBe("daemon");
  });

  it("returns 'in-process' for 'in-process'", () => {
    expect(parsePtyMode("in-process")).toBe("in-process");
  });

  it("returns 'auto' for 'auto'", () => {
    expect(parsePtyMode("auto")).toBe("auto");
  });

  it("is case-insensitive", () => {
    expect(parsePtyMode("DAEMON")).toBe("daemon");
    expect(parsePtyMode("In-Process")).toBe("in-process");
    expect(parsePtyMode("AUTO")).toBe("auto");
  });

  it("trims whitespace", () => {
    expect(parsePtyMode("  daemon  ")).toBe("daemon");
    expect(parsePtyMode("\tin-process\n")).toBe("in-process");
  });

  it("returns 'auto' for invalid values", () => {
    expect(parsePtyMode("invalid")).toBe("auto");
    expect(parsePtyMode("daemn")).toBe("auto");
    expect(parsePtyMode("inprocess")).toBe("auto");
  });
});

// ---------------------------------------------------------------------------
// initializePtyMode — in-process mode
// ---------------------------------------------------------------------------

describe("initializePtyMode — in-process mode", () => {
  it("skips daemon entirely and resolves to in-process", async () => {
    const deps = createMockDeps();

    const result = await initializePtyMode(deps, "in-process");

    expect(result.resolvedMode).toBe("in-process");
    expect(result.configuredMode).toBe("in-process");
    expect(result.fallback).toBeUndefined();
  });

  it("does not call spawner or client", async () => {
    const deps = createMockDeps();

    await initializePtyMode(deps, "in-process");

    expect(deps.spawner.ensureRunning).not.toHaveBeenCalled();
    expect(deps.client.ensureConnected).not.toHaveBeenCalled();
  });

  it("registers crab:pty-mode IPC handler", async () => {
    const deps = createMockDeps();

    await initializePtyMode(deps, "in-process");

    expect(deps.ipcMain.handlers.has("crab:pty-mode")).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// initializePtyMode — daemon mode (required)
// ---------------------------------------------------------------------------

describe("initializePtyMode — daemon mode (required)", () => {
  it("connects to daemon and resolves to daemon mode", async () => {
    const deps = createMockDeps();

    const result = await initializePtyMode(deps, "daemon");

    expect(result.resolvedMode).toBe("daemon");
    expect(result.configuredMode).toBe("daemon");
    expect(result.fallback).toBeUndefined();
  });

  it("calls spawner.ensureRunning and client.ensureConnected", async () => {
    const deps = createMockDeps();

    await initializePtyMode(deps, "daemon");

    expect(deps.spawner.ensureRunning).toHaveBeenCalledOnce();
    expect(deps.client.ensureConnected).toHaveBeenCalledOnce();
  });

  it("throws when spawner fails", async () => {
    const deps = createMockDeps({ spawnerFails: true, spawnerError: "Socket not found" });

    await expect(initializePtyMode(deps, "daemon")).rejects.toThrow(
      "CRAB_PTY_MODE=daemon but daemon is unavailable: Socket not found",
    );
  });

  it("throws when client connection fails", async () => {
    const deps = createMockDeps({ clientFails: true, clientError: "Auth rejected" });

    await expect(initializePtyMode(deps, "daemon")).rejects.toThrow(
      "CRAB_PTY_MODE=daemon but daemon is unavailable: Auth rejected",
    );
  });

  it("does not fall back to in-process on failure", async () => {
    const deps = createMockDeps({ spawnerFails: true });

    try {
      await initializePtyMode(deps, "daemon");
    } catch {
      // Expected to throw
    }

    // IPC handler should not be registered on failure
    expect(deps.ipcMain.handlers.has("crab:pty-mode")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// initializePtyMode — auto mode
// ---------------------------------------------------------------------------

describe("initializePtyMode — auto mode", () => {
  it("connects to daemon when available", async () => {
    const deps = createMockDeps();

    const result = await initializePtyMode(deps, "auto");

    expect(result.resolvedMode).toBe("daemon");
    expect(result.configuredMode).toBe("auto");
    expect(result.fallback).toBeUndefined();
  });

  it("falls back to in-process when spawner fails", async () => {
    const deps = createMockDeps({ spawnerFails: true, spawnerError: "No socket" });

    const result = await initializePtyMode(deps, "auto");

    expect(result.resolvedMode).toBe("in-process");
    expect(result.configuredMode).toBe("auto");
    expect(result.fallback).toBeDefined();
    expect(result.fallback!.reason).toBe("Daemon connection failed");
    expect(result.fallback!.error).toBe("No socket");
    expect(result.fallback!.timestamp).toBeGreaterThan(0);
  });

  it("falls back to in-process when client connection fails", async () => {
    const deps = createMockDeps({ clientFails: true, clientError: "Handshake timeout" });

    const result = await initializePtyMode(deps, "auto");

    expect(result.resolvedMode).toBe("in-process");
    expect(result.configuredMode).toBe("auto");
    expect(result.fallback).toBeDefined();
    expect(result.fallback!.error).toBe("Handshake timeout");
  });

  it("logs a warning on fallback", async () => {
    const deps = createMockDeps({ spawnerFails: true });

    await initializePtyMode(deps, "auto");

    expect(deps.log).toHaveBeenCalledWith(
      "warn",
      expect.stringContaining("falling back to in-process"),
    );
  });

  it("uses auto as default when env var is undefined", async () => {
    const deps = createMockDeps();

    const result = await initializePtyMode(deps, undefined);

    expect(result.configuredMode).toBe("auto");
    expect(result.resolvedMode).toBe("daemon");
  });
});

// ---------------------------------------------------------------------------
// registerPtyModeIpc
// ---------------------------------------------------------------------------

describe("registerPtyModeIpc", () => {
  it("registers crab:pty-mode handler that returns mode info", async () => {
    const ipcMain = createMockIpcMain();
    const result: ModeSelectionResult = {
      resolvedMode: "daemon",
      configuredMode: "auto",
    };

    registerPtyModeIpc(ipcMain, result);

    const response = await ipcMain.invoke("crab:pty-mode");
    expect(response).toEqual({
      resolvedMode: "daemon",
      configuredMode: "auto",
      fallback: null,
    });
  });

  it("includes fallback info when present", async () => {
    const ipcMain = createMockIpcMain();
    const result: ModeSelectionResult = {
      resolvedMode: "in-process",
      configuredMode: "auto",
      fallback: {
        reason: "Daemon connection failed",
        error: "Socket not found",
        timestamp: 1700000000000,
      },
    };

    registerPtyModeIpc(ipcMain, result);

    const response = await ipcMain.invoke("crab:pty-mode");
    expect(response).toEqual({
      resolvedMode: "in-process",
      configuredMode: "auto",
      fallback: {
        reason: "Daemon connection failed",
        error: "Socket not found",
        timestamp: 1700000000000,
      },
    });
  });
});
