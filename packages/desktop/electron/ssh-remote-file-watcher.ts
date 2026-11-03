/**
 * Remote File Watcher — watches remote file system changes via the
 * agent's inotify/FSEvents/polling backend and routes events to the
 * appropriate consumer (WorkspaceSyncEngine or file browser).
 *
 * Responsibilities:
 * - Start/stop remote file watching via agent RPC (watch_start/watch_stop)
 * - Route fs_event notifications to WorkspaceSyncEngine when sync is active
 * - Emit "remote-file-changed" events for the file browser when sync is inactive
 * - Handle polling fallback notification when >50,000 files
 * - Auto-reconnect watching within 10s of connection restoration
 * - Update file browser tree within 2s of change detection
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { WorkspaceSyncEngine } from "./ssh-workspace-sync";

// ── Types ────────────────────────────────────────────────────────

export type WatchMode = "inotify" | "fsevents" | "polling";

export interface WatchOptions {
  recursive?: boolean;
  ignorePatterns?: string[];
}

export interface WatchStartResult {
  watchId: string;
  mode: WatchMode;
  fileCount: number;
}

export interface RemoteFileEvent {
  path: string;
  kind: "create" | "modify" | "delete" | "rename";
  timestamp: number;
}

interface ActiveWatch {
  watchId: string;
  profileId: string;
  remotePath: string;
  mode: WatchMode;
  options: WatchOptions;
}

// ── Constants ────────────────────────────────────────────────────

/** Maximum time to re-establish watching after reconnection (ms). */
const RECONNECT_WATCH_TIMEOUT_MS = 10_000;

/** Maximum time for file browser tree update after change detection (ms). */
const FILE_BROWSER_UPDATE_DELAY_MS = 200;

/** Polling fallback threshold (file count). */
const POLLING_FALLBACK_THRESHOLD = 50_000;

// ── RemoteFileWatcher ────────────────────────────────────────────

/**
 * Manages remote file system watching via the agent RPC protocol.
 *
 * Events emitted:
 * - "remote-file-changed" ({ profileId, path, event }) — for file browser updates
 * - "watch-mode-changed" ({ profileId, watchId, mode }) — when mode changes (e.g., polling fallback)
 * - "polling-fallback" ({ profileId, watchId, fileCount }) — when >50,000 files triggers polling
 * - "watch-error" ({ profileId, watchId, error }) — on watch failures
 * - "watch-reconnected" ({ profileId, watchId }) — after successful reconnection
 */
export class RemoteFileWatcher extends EventEmitter {
  private activeWatches = new Map<string, ActiveWatch>();
  private notificationHandler: ((notification: {
    profileId: string;
    method: string;
    params: unknown;
  }) => void) | null = null;
  private statusHandler: ((payload: {
    profileId: string;
    status: string;
    error?: string;
  }) => void) | null = null;
  private reconnectTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private fileBrowserUpdateTimers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Tracks watches that need to be re-established after reconnection, keyed by profileId. */
  private pendingReconnectWatches = new Map<string, ActiveWatch[]>();

  constructor(
    private router: RpcRouter,
    private sessionManager: SshSessionManager,
    private syncEngine?: WorkspaceSyncEngine,
  ) {
    super();
    this.setupNotificationListener();
    this.setupReconnectionListener();
  }

  // ── Public API ──────────────────────────────────────────────────

  /**
   * Start watching a remote path for file system events.
   * Calls the agent's "watch_start" RPC method.
   *
   * Returns the watch mode selected by the agent (inotify/fsevents/polling).
   */
  async startWatching(
    profileId: string,
    remotePath: string,
    options: WatchOptions = {},
  ): Promise<WatchStartResult> {
    const params = {
      path: remotePath,
      recursive: options.recursive ?? true,
      ignore_patterns: options.ignorePatterns ?? [],
    };

    const result = await this.router.sendToRemoteAgent(profileId, "watch_start", params) as {
      watch_id: string;
      mode: WatchMode;
      file_count: number;
    };

    const watch: ActiveWatch = {
      watchId: result.watch_id,
      profileId,
      remotePath,
      mode: result.mode,
      options,
    };

    this.activeWatches.set(result.watch_id, watch);

    // Emit polling fallback notification if file count exceeds threshold
    if (result.file_count > POLLING_FALLBACK_THRESHOLD || result.mode === "polling") {
      this.emit("polling-fallback", {
        profileId,
        watchId: result.watch_id,
        fileCount: result.file_count,
      });
    }

    return {
      watchId: result.watch_id,
      mode: result.mode,
      fileCount: result.file_count,
    };
  }

  /**
   * Stop watching a remote path.
   * Calls the agent's "watch_stop" RPC method.
   */
  async stopWatching(profileId: string, watchId: string): Promise<void> {
    try {
      await this.router.sendToRemoteAgent(profileId, "watch_stop", {
        watch_id: watchId,
      });
    } catch {
      // Best-effort: connection may already be gone
    }

    this.activeWatches.delete(watchId);
    this.clearFileBrowserTimer(watchId);
  }

  /**
   * Stop all active watches for a profile.
   */
  async stopAllForProfile(profileId: string): Promise<void> {
    const watchesToStop: string[] = [];
    for (const [watchId, watch] of this.activeWatches) {
      if (watch.profileId === profileId) {
        watchesToStop.push(watchId);
      }
    }

    await Promise.all(
      watchesToStop.map((watchId) => this.stopWatching(profileId, watchId)),
    );
  }

  /**
   * Get all active watches for a profile.
   */
  getActiveWatches(profileId: string): ActiveWatch[] {
    const watches: ActiveWatch[] = [];
    for (const watch of this.activeWatches.values()) {
      if (watch.profileId === profileId) {
        watches.push(watch);
      }
    }
    return watches;
  }

  /**
   * Check if a specific path is being watched for a profile.
   */
  isWatching(profileId: string, remotePath: string): boolean {
    for (const watch of this.activeWatches.values()) {
      if (watch.profileId === profileId && watch.remotePath === remotePath) {
        return true;
      }
    }
    return false;
  }

  /**
   * Clean up all resources. Call during app shutdown.
   */
  dispose(): void {
    // Remove event listeners
    if (this.notificationHandler) {
      this.router.removeListener("notification", this.notificationHandler);
      this.notificationHandler = null;
    }
    if (this.statusHandler) {
      this.sessionManager.removeListener("ssh:status-changed", this.statusHandler);
      this.statusHandler = null;
    }

    // Clear all timers
    for (const timer of this.reconnectTimers.values()) {
      clearTimeout(timer);
    }
    this.reconnectTimers.clear();

    for (const timer of this.fileBrowserUpdateTimers.values()) {
      clearTimeout(timer);
    }
    this.fileBrowserUpdateTimers.clear();

    // Stop all watches (best-effort, don't await)
    for (const [watchId, watch] of this.activeWatches) {
      this.router.sendToRemoteAgent(watch.profileId, "watch_stop", {
        watch_id: watchId,
      }).catch(() => {
        // Connection may already be gone
      });
    }
    this.activeWatches.clear();
    this.pendingReconnectWatches.clear();
  }

  // ── Notification Handling ───────────────────────────────────────

  /**
   * Set up listener for RPC notifications from the remote agent.
   * Routes "fs_event" notifications to the appropriate consumer.
   */
  private setupNotificationListener(): void {
    this.notificationHandler = (notification) => {
      if (notification.method === "fs_event") {
        this.handleFsEvent(notification.profileId, notification.params);
      }
    };
    this.router.on("notification", this.notificationHandler);
  }

  /**
   * Handle an incoming fs_event notification from the remote agent.
   * Routes events to WorkspaceSyncEngine if sync is active for the
   * directory, otherwise emits for the file browser.
   */
  private handleFsEvent(profileId: string, params: unknown): void {
    const event = params as {
      watch_id: string;
      events: Array<{ path: string; kind: "create" | "modify" | "delete" | "rename"; timestamp: number }>;
    };

    const watch = this.activeWatches.get(event.watch_id);
    if (!watch) return;

    // Check if WorkspaceSyncEngine is active for this directory
    if (this.syncEngine && this.isSyncActiveForPath(watch.profileId, watch.remotePath)) {
      // Route to sync engine — it handles its own event processing
      // The sync engine already listens for "fs_event" notifications directly,
      // so we don't need to forward. But per requirement 11.7, we should NOT
      // also emit to the file browser to avoid duplicate notifications.
      return;
    }

    // Route to file browser: emit individual events within 2s of detection
    for (const fsEvent of event.events) {
      this.scheduleFileBrowserUpdate(profileId, fsEvent);
    }
  }

  /**
   * Schedule a file browser tree update within 2s of change detection.
   * Batches rapid events to avoid excessive UI updates.
   */
  private scheduleFileBrowserUpdate(
    profileId: string,
    event: RemoteFileEvent,
  ): void {
    // Emit immediately (well within the 2s requirement) with a small
    // debounce to batch rapid successive events for the same file
    const key = `${profileId}:${event.path}`;

    // Clear any existing timer for this specific file
    const existingTimer = this.fileBrowserUpdateTimers.get(key);
    if (existingTimer) {
      clearTimeout(existingTimer);
    }

    // Schedule emission with a short delay to batch rapid events
    const timer = setTimeout(() => {
      this.fileBrowserUpdateTimers.delete(key);
      this.emit("remote-file-changed", {
        profileId,
        path: event.path,
        event: event.kind,
        timestamp: event.timestamp,
      });
    }, FILE_BROWSER_UPDATE_DELAY_MS);

    this.fileBrowserUpdateTimers.set(key, timer);
  }

  // ── Reconnection Handling ───────────────────────────────────────

  /**
   * Listen for SSH session status changes to detect disconnection
   * and reconnection. Re-establishes watches within 10s of
   * connection restoration.
   */
  private setupReconnectionListener(): void {
    this.statusHandler = (payload) => {
      const { profileId, status } = payload;

      if (status === "reconnecting" || status === "disconnected") {
        this.handleDisconnection(profileId);
      } else if (status === "connected") {
        this.handleReconnection(profileId);
      }
    };
    this.sessionManager.on("ssh:status-changed", this.statusHandler);
  }

  /**
   * Handle disconnection: preserve watch state for re-establishment
   * after reconnection.
   */
  private handleDisconnection(profileId: string): void {
    const watchesToRestore: ActiveWatch[] = [];

    for (const [watchId, watch] of this.activeWatches) {
      if (watch.profileId === profileId) {
        watchesToRestore.push({ ...watch });
        this.activeWatches.delete(watchId);
        this.clearFileBrowserTimer(watchId);
      }
    }

    if (watchesToRestore.length > 0) {
      this.pendingReconnectWatches.set(profileId, watchesToRestore);
    }
  }

  /**
   * Handle reconnection: re-establish all watches that were active
   * before disconnection, within 10s of connection restoration.
   */
  private handleReconnection(profileId: string): void {
    const watchesToRestore = this.pendingReconnectWatches.get(profileId);
    if (!watchesToRestore || watchesToRestore.length === 0) return;

    this.pendingReconnectWatches.delete(profileId);

    // Set a timeout to ensure we complete within 10s
    const timeoutTimer = setTimeout(() => {
      this.reconnectTimers.delete(profileId);
      this.emit("watch-error", {
        profileId,
        watchId: null,
        error: "Failed to re-establish watches within 10s timeout",
      });
    }, RECONNECT_WATCH_TIMEOUT_MS);

    this.reconnectTimers.set(profileId, timeoutTimer);

    // Re-establish all watches
    this.restoreWatches(profileId, watchesToRestore, timeoutTimer);
  }

  /**
   * Restore watches after reconnection. Attempts to re-establish each
   * watch and emits appropriate events on success or failure.
   */
  private async restoreWatches(
    profileId: string,
    watches: ActiveWatch[],
    timeoutTimer: ReturnType<typeof setTimeout>,
  ): Promise<void> {
    let allSucceeded = true;

    for (const watch of watches) {
      try {
        const result = await this.startWatching(
          profileId,
          watch.remotePath,
          watch.options,
        );

        this.emit("watch-reconnected", {
          profileId,
          watchId: result.watchId,
          previousWatchId: watch.watchId,
        });

        // Notify if mode changed (e.g., switched to polling after reconnect)
        if (result.mode !== watch.mode) {
          this.emit("watch-mode-changed", {
            profileId,
            watchId: result.watchId,
            mode: result.mode,
            previousMode: watch.mode,
          });
        }
      } catch (err) {
        allSucceeded = false;
        this.emit("watch-error", {
          profileId,
          watchId: watch.watchId,
          error: `Failed to restore watch for ${watch.remotePath}: ${err instanceof Error ? err.message : String(err)}`,
        });
      }
    }

    // Clear the timeout since we finished (success or partial failure)
    clearTimeout(timeoutTimer);
    this.reconnectTimers.delete(profileId);

    if (allSucceeded) {
      this.emit("watches-restored", { profileId, count: watches.length });
    }
  }

  // ── Helpers ────────────────────────────────────────────────────

  /**
   * Check if WorkspaceSyncEngine has an active sync pair for the given
   * remote path. When sync is active, file events should be routed to
   * the sync engine rather than the file browser.
   */
  private isSyncActiveForPath(profileId: string, remotePath: string): boolean {
    if (!this.syncEngine) return false;

    const syncPairs = this.syncEngine.listSyncPairs(profileId);
    for (const pair of syncPairs) {
      if (pair.status === "active" && this.isPathWithin(remotePath, pair.remoteDir)) {
        return true;
      }
    }
    return false;
  }

  /**
   * Check if a path is within (or equal to) a base directory.
   */
  private isPathWithin(path: string, baseDir: string): boolean {
    const normalizedPath = path.replace(/\/$/, "");
    const normalizedBase = baseDir.replace(/\/$/, "");
    return normalizedPath === normalizedBase || normalizedPath.startsWith(normalizedBase + "/");
  }

  /**
   * Clear file browser update timer for a specific watch.
   */
  private clearFileBrowserTimer(watchId: string): void {
    // Clear all timers associated with this watch's events
    for (const [key, timer] of this.fileBrowserUpdateTimers) {
      if (key.includes(watchId)) {
        clearTimeout(timer);
        this.fileBrowserUpdateTimers.delete(key);
      }
    }
  }
}
