/**
 * Remote Process Manager — lists, monitors, and signals processes on
 * the remote server via the agent RPC interface.
 *
 * Polls the remote agent at 5-second intervals when active, emitting
 * "process-list-updated" events with the latest process list. Handles
 * agent unreachable gracefully by emitting an "error" event rather
 * than throwing, and retries on the next polling interval.
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteProcess } from "./ssh-types";

// ── Constants ────────────────────────────────────────────────────

/** Polling interval for process list refresh (milliseconds). */
const POLLING_INTERVAL_MS = 5_000;

/** Maximum number of processes to request from the remote agent. */
const MAX_PROCESSES = 500;

// ── Types ────────────────────────────────────────────────────────

export interface RemoteProcessManagerEvents {
  "process-list-updated": (payload: { profileId: string; processes: RemoteProcess[] }) => void;
  error: (payload: { profileId: string; error: string }) => void;
}

// ── RemoteProcessManager ─────────────────────────────────────────

export class RemoteProcessManager extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  private pollingTimers = new Map<string, ReturnType<typeof setInterval>>();

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;
  }

  /**
   * Fetch the process list from the remote agent for the given profile.
   *
   * Returns up to 500 processes with PID, name, CPU%, memory%, memory
   * bytes, owning user, and runtime. Emits an "error" event and returns
   * an empty array if the agent is unreachable.
   */
  async listProcesses(profileId: string): Promise<RemoteProcess[]> {
    try {
      const status = this.sessionManager.getStatus(profileId);
      if (status !== "connected") {
        this.emit("error", {
          profileId,
          error: `Agent unreachable: connection status is "${status}"`,
        });
        return [];
      }

      const result = await this.router.sendToRemoteAgent(profileId, "process_list", {
        limit: MAX_PROCESSES,
      });

      const response = result as { processes?: RemoteProcess[] };
      const processes = response.processes ?? [];

      return processes.slice(0, MAX_PROCESSES);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.emit("error", { profileId, error: message });
      return [];
    }
  }

  /**
   * Send a signal (SIGTERM or SIGKILL) to a process on the remote server.
   *
   * Handles permission errors gracefully by emitting an "error" event
   * with a descriptive message rather than throwing.
   */
  async signalProcess(
    profileId: string,
    pid: number,
    signal: "SIGTERM" | "SIGKILL",
  ): Promise<void> {
    try {
      const status = this.sessionManager.getStatus(profileId);
      if (status !== "connected") {
        this.emit("error", {
          profileId,
          error: `Cannot signal process: connection status is "${status}"`,
        });
        return;
      }

      await this.router.sendToRemoteAgent(profileId, "process_signal", {
        pid,
        signal,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);

      // Provide a user-friendly message for permission errors
      const isPermissionError =
        message.toLowerCase().includes("permission") ||
        message.toLowerCase().includes("eperm") ||
        message.toLowerCase().includes("operation not permitted");

      const errorMessage = isPermissionError
        ? `Permission denied: cannot send ${signal} to process ${pid}. The process may be owned by another user.`
        : `Failed to signal process ${pid}: ${message}`;

      this.emit("error", { profileId, error: errorMessage });
    }
  }

  /**
   * Start periodic polling of the process list for a profile.
   *
   * Polls every 5 seconds and emits "process-list-updated" events
   * with the latest process list. If the agent is unreachable during
   * a poll, emits an "error" event and retries on the next interval.
   *
   * Calling startPolling when already polling for the same profile
   * is a no-op.
   */
  startPolling(profileId: string): void {
    if (this.pollingTimers.has(profileId)) {
      return;
    }

    // Perform an initial fetch immediately
    this.poll(profileId);

    const timer = setInterval(() => {
      this.poll(profileId);
    }, POLLING_INTERVAL_MS);

    this.pollingTimers.set(profileId, timer);
  }

  /**
   * Stop periodic polling of the process list for a profile.
   *
   * Calling stopPolling when not polling for the given profile is a no-op.
   */
  stopPolling(profileId: string): void {
    const timer = this.pollingTimers.get(profileId);
    if (timer) {
      clearInterval(timer);
      this.pollingTimers.delete(profileId);
    }
  }

  /**
   * Stop all active polling timers. Useful during graceful shutdown.
   */
  stopAllPolling(): void {
    for (const [, timer] of this.pollingTimers) {
      clearInterval(timer);
    }
    this.pollingTimers.clear();
  }

  /**
   * Check whether polling is active for a given profile.
   */
  isPolling(profileId: string): boolean {
    return this.pollingTimers.has(profileId);
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Execute a single poll: fetch the process list and emit the result.
   */
  private async poll(profileId: string): Promise<void> {
    const processes = await this.listProcesses(profileId);
    this.emit("process-list-updated", { profileId, processes });
  }
}
