/**
 * Mode Selection — reads CRAB_PTY_MODE env var and determines whether
 * to use the daemon, fall back to in-process, or skip the daemon entirely.
 *
 * Modes:
 * - `auto` (default): attempt daemon, fall back to in-process on failure
 * - `daemon`: require daemon, throw on failure
 * - `in-process`: legacy mode, no daemon
 *
 * Exposes the resolved mode via `crab:pty-mode` IPC for diagnostics.
 */

import type { DaemonApi } from "./daemon-client";
import type { DaemonSpawner } from "./daemon-spawner";
import type { IpcMainLike } from "./ipc-bridge";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** The configured mode from the env var. */
export type PtyModeConfig = "auto" | "daemon" | "in-process";

/** The resolved runtime mode after initialization. */
export type PtyModeResolved = "daemon" | "in-process";

/** Reason for fallback when auto mode falls back to in-process. */
export interface FallbackInfo {
  reason: string;
  error?: string;
  timestamp: number;
}

export interface ModeSelectionResult {
  /** The resolved mode that is actually in use. */
  resolvedMode: PtyModeResolved;
  /** The originally configured mode from the env var. */
  configuredMode: PtyModeConfig;
  /** If auto mode fell back, the reason why. */
  fallback?: FallbackInfo;
}

export interface ModeSelectionDeps {
  /** The DaemonClient instance. */
  client: DaemonApi;
  /** The DaemonSpawner instance. */
  spawner: DaemonSpawner;
  /** Electron ipcMain for registering the mode IPC handler. */
  ipcMain: IpcMainLike;
  /** Logger for warnings. */
  log?: (level: "warn" | "info" | "error", message: string) => void;
}

// ---------------------------------------------------------------------------
// Env var parsing
// ---------------------------------------------------------------------------

/**
 * Parse the CRAB_PTY_MODE env var. Returns 'auto' if unset or invalid.
 */
export function parsePtyMode(envValue: string | undefined): PtyModeConfig {
  if (!envValue) return "auto";

  const normalized = envValue.trim().toLowerCase();
  if (normalized === "daemon" || normalized === "in-process" || normalized === "auto") {
    return normalized;
  }

  // Invalid value — treat as auto with a warning
  return "auto";
}

// ---------------------------------------------------------------------------
// Mode initialization
// ---------------------------------------------------------------------------

/**
 * Initialize the PTY mode based on the CRAB_PTY_MODE env var.
 *
 * - `auto`: attempt to connect/spawn the daemon. On failure, fall back
 *   to in-process mode with a warning.
 * - `daemon`: attempt to connect/spawn the daemon. On failure, throw.
 * - `in-process`: skip daemon entirely, return in-process mode immediately.
 *
 * After resolution, registers the `crab:pty-mode` IPC handler so the
 * renderer can query the active mode for diagnostics.
 */
export async function initializePtyMode(
  deps: ModeSelectionDeps,
  envValue?: string,
): Promise<ModeSelectionResult> {
  const configuredMode = parsePtyMode(envValue ?? process.env.CRAB_PTY_MODE);
  const { client, spawner, ipcMain, log } = deps;

  let result: ModeSelectionResult;

  switch (configuredMode) {
    case "in-process": {
      log?.("info", "PTY mode: in-process (daemon skipped)");
      result = {
        resolvedMode: "in-process",
        configuredMode,
      };
      break;
    }

    case "daemon": {
      // Require daemon — throw on failure
      try {
        await spawner.ensureRunning();
        await client.ensureConnected();
        log?.("info", "PTY mode: daemon (required, connected)");
        result = {
          resolvedMode: "daemon",
          configuredMode,
        };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        log?.("error", `PTY mode: daemon required but failed: ${message}`);
        throw new Error(
          `CRAB_PTY_MODE=daemon but daemon is unavailable: ${message}`,
        );
      }
      break;
    }

    case "auto":
    default: {
      // Try daemon, fall back to in-process on failure
      try {
        await spawner.ensureRunning();
        await client.ensureConnected();
        log?.("info", "PTY mode: daemon (auto, connected)");
        result = {
          resolvedMode: "daemon",
          configuredMode,
        };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        log?.("warn", `PTY mode: falling back to in-process — daemon unavailable: ${message}`);
        result = {
          resolvedMode: "in-process",
          configuredMode,
          fallback: {
            reason: "Daemon connection failed",
            error: message,
            timestamp: Date.now(),
          },
        };
      }
      break;
    }
  }

  // Register the crab:pty-mode IPC handler for diagnostics
  registerPtyModeIpc(ipcMain, result);

  return result;
}

// ---------------------------------------------------------------------------
// IPC handler
// ---------------------------------------------------------------------------

/**
 * Register the `crab:pty-mode` IPC handler that exposes the resolved
 * mode to the renderer for diagnostics.
 */
export function registerPtyModeIpc(
  ipcMain: IpcMainLike,
  result: ModeSelectionResult,
): void {
  ipcMain.handle("crab:pty-mode", () => ({
    resolvedMode: result.resolvedMode,
    configuredMode: result.configuredMode,
    fallback: result.fallback ?? null,
  }));
}
