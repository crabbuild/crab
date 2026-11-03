/**
 * Session Persistence — saves active SSH connection state on app quit
 * and provides the list of profiles to auto-reconnect on restart.
 *
 * The persistence module is intentionally passive: it provides the list
 * of profile IDs that should be reconnected, but the caller decides
 * when and how to reconnect (typically after the SSH subsystem is
 * fully initialized).
 *
 * State file: ~/.crab/desktop/ssh-state.json
 * Format: { version: 1, activeConnections: string[], savedAt: number }
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import type { SshSessionManager } from "./ssh-manager";
import type { SshProfileManager } from "./ssh-profiles";

// ── Types ────────────────────────────────────────────────────────

export interface SshSessionState {
  version: 1;
  activeConnections: string[];
  savedAt: number;
}

// ── Constants ────────────────────────────────────────────────────

const DEFAULT_STATE_PATH = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "ssh-state.json",
);

// ── Session Persistence ──────────────────────────────────────────

/**
 * Manages persistence of active SSH connections across app restarts.
 *
 * On quit: saves the list of currently connected profile IDs.
 * On start: reads the saved state and returns profile IDs that have
 * `autoReconnect` enabled in their profile settings.
 *
 * The class does NOT initiate reconnection itself — it provides the
 * filtered list of profile IDs to the caller, who decides when to
 * reconnect (after SSH subsystem initialization).
 */
export class SessionPersistence {
  private statePath: string;
  private sessionManager: SshSessionManager;
  private profileManager: SshProfileManager;
  private beforeQuitHandler: (() => void) | null = null;

  constructor(
    sessionManager: SshSessionManager,
    profileManager: SshProfileManager,
    statePath?: string,
  ) {
    this.sessionManager = sessionManager;
    this.profileManager = profileManager;
    this.statePath = statePath ?? DEFAULT_STATE_PATH;
  }

  /**
   * Register the `before-quit` handler with Electron's app module.
   * Safe to call in non-Electron environments (e.g., tests) — uses
   * try/catch around the electron import.
   */
  registerQuitHandler(): void {
    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const { app } = require("electron") as typeof import("electron");

      this.beforeQuitHandler = () => {
        this.saveState();
      };

      app.on("before-quit", this.beforeQuitHandler);
    } catch {
      // Not running in Electron (e.g., unit tests) — skip handler registration
    }
  }

  /**
   * Unregister the `before-quit` handler. Called during cleanup.
   */
  unregisterQuitHandler(): void {
    if (!this.beforeQuitHandler) return;

    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const { app } = require("electron") as typeof import("electron");
      app.removeListener("before-quit", this.beforeQuitHandler);
    } catch {
      // Not running in Electron
    }

    this.beforeQuitHandler = null;
  }

  /**
   * Save the current active connection profile IDs to the state file.
   * Called automatically on app quit (via `before-quit` event) or
   * manually when needed.
   */
  saveState(): void {
    const connections = this.sessionManager.getAllConnections();
    const activeProfileIds = connections
      .filter((conn) => conn.status === "connected" || conn.status === "reconnecting")
      .map((conn) => conn.profile.id);

    const state: SshSessionState = {
      version: 1,
      activeConnections: activeProfileIds,
      savedAt: Date.now(),
    };

    try {
      const dir = path.dirname(this.statePath);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(this.statePath, JSON.stringify(state, null, 2), "utf-8");
    } catch {
      // Best-effort persistence — don't crash on write failure
    }
  }

  /**
   * Load saved session state and return the list of profile IDs that
   * should be auto-reconnected. Filters by:
   * 1. Profile still exists in the profile manager
   * 2. Profile has `autoReconnect: true`
   *
   * Returns an empty array if the state file is missing, corrupted,
   * or contains no valid profiles.
   */
  getProfileIdsToReconnect(): string[] {
    const state = this.loadState();
    if (!state) return [];

    return state.activeConnections.filter((profileId) => {
      const profile = this.profileManager.get(profileId);
      if (!profile) return false;
      return profile.autoReconnect;
    });
  }

  /**
   * Clear the state file. Called after successful reconnection of all
   * saved profiles, or on explicit disconnect-all.
   */
  clearState(): void {
    try {
      fs.unlinkSync(this.statePath);
    } catch {
      // File doesn't exist or can't be deleted — that's fine
    }
  }

  /**
   * Remove a specific profile ID from the persisted state. Called when
   * a user explicitly disconnects a single profile (so it won't be
   * auto-reconnected on next restart).
   */
  removeProfile(profileId: string): void {
    const state = this.loadState();
    if (!state) return;

    const filtered = state.activeConnections.filter((id) => id !== profileId);
    if (filtered.length === 0) {
      this.clearState();
      return;
    }

    const updated: SshSessionState = {
      version: 1,
      activeConnections: filtered,
      savedAt: state.savedAt,
    };

    try {
      fs.writeFileSync(this.statePath, JSON.stringify(updated, null, 2), "utf-8");
    } catch {
      // Best-effort — don't crash
    }
  }

  // ── Internal ─────────────────────────────────────────────────

  /**
   * Load and parse the state file. Returns null if the file is
   * missing, corrupted, or has an unsupported version.
   */
  private loadState(): SshSessionState | null {
    try {
      const content = fs.readFileSync(this.statePath, "utf-8");
      const parsed = JSON.parse(content);

      // Validate structure
      if (!parsed || typeof parsed !== "object") return null;
      if (parsed.version !== 1) return null;
      if (!Array.isArray(parsed.activeConnections)) return null;

      // Filter out non-string entries
      const activeConnections = parsed.activeConnections.filter(
        (id: unknown): id is string => typeof id === "string",
      );

      return {
        version: 1,
        activeConnections,
        savedAt: typeof parsed.savedAt === "number" ? parsed.savedAt : 0,
      };
    } catch {
      // File missing, unreadable, or invalid JSON — start fresh
      return null;
    }
  }
}
