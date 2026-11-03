/**
 * Auto-Respawn — handles automatic daemon respawn on unexpected death.
 *
 * When the DaemonClient detects a disconnect (onDisconnected fires),
 * this module:
 * 1. Synthesizes pty:exit events for all active sessions (so the
 *    renderer knows they're gone before teardown).
 * 2. Resets the spawner handle.
 * 3. Coalesces concurrent respawn attempts via a shared promise.
 * 4. Calls ensureRunning() to respawn the daemon.
 * 5. Provides retry-once semantics for failed operations after respawn.
 * 6. Exposes a public restartDaemon() method for explicit user-triggered
 *    restart (e.g., from diagnostics UI).
 */

import type { DaemonSpawner } from "./daemon-spawner";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface AutoRespawnDeps {
  /** The DaemonSpawner instance for resetting and respawning. */
  spawner: DaemonSpawner;
  /** Returns all currently tracked session IDs. */
  getActiveSessionIds: () => string[];
  /** Synthesize a pty:exit event for a session (notify renderer). */
  synthesizeExitEvent: (sessionId: string, code: number) => void;
  /** Reconnect the client after daemon respawn. */
  reconnect: () => Promise<void>;
  /** Logger for respawn events. */
  log?: (level: "info" | "warn" | "error", message: string) => void;
}

export interface AutoRespawnOptions {
  /** Maximum number of respawn attempts before giving up. Default: 3. */
  maxAttempts?: number;
  /** Delay between respawn attempts (ms). Default: 1000. */
  retryDelayMs?: number;
  /** Whether auto-respawn is enabled. Default: true. */
  enabled?: boolean;
}

export type RespawnState = "idle" | "respawning" | "failed";

// ---------------------------------------------------------------------------
// AutoRespawn
// ---------------------------------------------------------------------------

export class AutoRespawn {
  private readonly spawner: DaemonSpawner;
  private readonly getActiveSessionIds: () => string[];
  private readonly synthesizeExitEvent: (sessionId: string, code: number) => void;
  private readonly reconnect: () => Promise<void>;
  private readonly log: (level: "info" | "warn" | "error", message: string) => void;

  private readonly maxAttempts: number;
  private readonly retryDelayMs: number;
  private readonly enabled: boolean;

  private _state: RespawnState = "idle";
  private respawnPromise: Promise<boolean> | null = null;
  private respawnCount = 0;

  constructor(deps: AutoRespawnDeps, opts: AutoRespawnOptions = {}) {
    this.spawner = deps.spawner;
    this.getActiveSessionIds = deps.getActiveSessionIds;
    this.synthesizeExitEvent = deps.synthesizeExitEvent;
    this.reconnect = deps.reconnect;
    this.log = deps.log ?? (() => {});

    this.maxAttempts = opts.maxAttempts ?? 3;
    this.retryDelayMs = opts.retryDelayMs ?? 1000;
    this.enabled = opts.enabled ?? true;
  }

  /** Current respawn state. */
  get state(): RespawnState {
    return this._state;
  }

  /** Number of times the daemon has been respawned. */
  get totalRespawns(): number {
    return this.respawnCount;
  }

  /**
   * Handle daemon disconnection. Called from DaemonClient.onDisconnected.
   *
   * Synthesizes exit events for active sessions, then attempts to
   * respawn the daemon. Concurrent calls are coalesced via a shared
   * promise — multiple disconnect events won't spawn multiple daemons.
   *
   * @returns true if respawn succeeded, false if it failed or is disabled.
   */
  async handleDisconnect(): Promise<boolean> {
    if (!this.enabled) {
      this.log("info", "Auto-respawn disabled, skipping respawn");
      return false;
    }

    // Coalesce concurrent respawn attempts
    if (this.respawnPromise) {
      this.log("info", "Respawn already in progress, coalescing");
      return this.respawnPromise;
    }

    this.respawnPromise = this.doRespawn();
    try {
      return await this.respawnPromise;
    } finally {
      this.respawnPromise = null;
    }
  }

  /**
   * Explicitly restart the daemon. Public method for user-triggered
   * restart (e.g., from diagnostics UI "Restart Daemon" button).
   *
   * Unlike handleDisconnect, this always attempts a fresh respawn
   * regardless of current state.
   *
   * @returns true if restart succeeded, false otherwise.
   */
  async restartDaemon(): Promise<boolean> {
    // If a respawn is already in progress, wait for it
    if (this.respawnPromise) {
      this.log("info", "Respawn already in progress, waiting for completion");
      return this.respawnPromise;
    }

    this.respawnPromise = this.doRestart();
    try {
      return await this.respawnPromise;
    } finally {
      this.respawnPromise = null;
    }
  }

  /**
   * Retry a failed operation once after a successful respawn.
   * Wraps an async operation with retry-on-disconnect semantics.
   *
   * @param operation The async operation to execute.
   * @returns The result of the operation.
   * @throws If the operation fails both before and after respawn.
   */
  async withRetryOnDisconnect<T>(operation: () => Promise<T>): Promise<T> {
    try {
      return await operation();
    } catch (err) {
      const isDisconnect = err instanceof Error &&
        (err.message.includes("Daemon connection lost") ||
         err.message.includes("Control socket not connected") ||
         err.message.includes("Client disconnected"));

      if (!isDisconnect) {
        throw err;
      }

      this.log("info", "Operation failed due to disconnect, attempting respawn and retry");

      const respawned = await this.handleDisconnect();
      if (!respawned) {
        throw err;
      }

      // Retry once after successful respawn
      return await operation();
    }
  }

  // -------------------------------------------------------------------------
  // Private
  // -------------------------------------------------------------------------

  private async doRespawn(): Promise<boolean> {
    this._state = "respawning";
    this.log("info", "Daemon disconnected, synthesizing exit events and attempting respawn");

    // Synthesize pty:exit events for all active sessions
    this.synthesizeExitEvents();

    // Reset spawner handle
    this.spawner.resetHandle();

    // Attempt respawn with retries
    for (let attempt = 1; attempt <= this.maxAttempts; attempt++) {
      try {
        this.log("info", `Respawn attempt ${attempt}/${this.maxAttempts}`);
        await this.spawner.ensureRunning();
        await this.reconnect();

        this._state = "idle";
        this.respawnCount++;
        this.log("info", "Daemon respawned and reconnected successfully");
        return true;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.log("warn", `Respawn attempt ${attempt} failed: ${message}`);

        if (attempt < this.maxAttempts) {
          await this.delay(this.retryDelayMs);
        }
      }
    }

    this._state = "failed";
    this.log("error", `Failed to respawn daemon after ${this.maxAttempts} attempts`);
    return false;
  }

  private async doRestart(): Promise<boolean> {
    this._state = "respawning";
    this.log("info", "Explicit daemon restart requested");

    // Synthesize exit events for active sessions
    this.synthesizeExitEvents();

    // Reset spawner handle
    this.spawner.resetHandle();

    // Attempt respawn with retries
    for (let attempt = 1; attempt <= this.maxAttempts; attempt++) {
      try {
        this.log("info", `Restart attempt ${attempt}/${this.maxAttempts}`);
        await this.spawner.ensureRunning();
        await this.reconnect();

        this._state = "idle";
        this.respawnCount++;
        this.log("info", "Daemon restarted and reconnected successfully");
        return true;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.log("warn", `Restart attempt ${attempt} failed: ${message}`);

        if (attempt < this.maxAttempts) {
          await this.delay(this.retryDelayMs);
        }
      }
    }

    this._state = "failed";
    this.log("error", `Failed to restart daemon after ${this.maxAttempts} attempts`);
    return false;
  }

  private synthesizeExitEvents(): void {
    const sessionIds = this.getActiveSessionIds();
    this.log("info", `Synthesizing exit events for ${sessionIds.length} active sessions`);

    for (const sessionId of sessionIds) {
      // Exit code -1 signals abnormal termination (daemon death)
      this.synthesizeExitEvent(sessionId, -1);
    }
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}
