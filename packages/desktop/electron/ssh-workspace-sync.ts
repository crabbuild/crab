/**
 * Workspace Sync Engine — bidirectional file synchronization between
 * local and remote directories with conflict detection.
 *
 * Monitors both local (via Node fs.watch) and remote (via agent RPC
 * "watch_start") directories for file system events. Propagates local
 * changes within 3 seconds and remote changes within 5 seconds.
 *
 * Conflict detection: if the same file is modified on both sides
 * between sync cycles, the file is flagged as conflicted and both
 * versions are retained (remote version gets a conflict suffix).
 *
 * Uses DeltaSyncEngine for files >= 1 MB to transfer only changed
 * chunks. Retries failed transfers with exponential backoff (1s, 2s,
 * 4s) up to 3 attempts.
 */

import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as path from "node:path";
import { minimatch } from "minimatch";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { DeltaSyncEngine } from "./ssh-delta-sync";
import type { FileTransferManager } from "./ssh-file-transfer";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type BetterSqlite3 from "better-sqlite3";
import type { SyncPair, SyncConflict } from "./ssh-types";

// ── Constants ────────────────────────────────────────────────────

/** Local change propagation target (milliseconds). */
const LOCAL_PROPAGATION_MS = 3_000;

/** Remote change propagation target (milliseconds). */
const REMOTE_PROPAGATION_MS = 5_000;

/** Minimum file size for delta sync (1 MB). */
const DELTA_SYNC_THRESHOLD = 1 * 1024 * 1024;

/** Maximum retry attempts on failure. */
const MAX_RETRIES = 3;

/** Base delay for exponential backoff (milliseconds). */
const BASE_RETRY_DELAY_MS = 1_000;

/** Debounce interval for local file events (milliseconds). */
const LOCAL_DEBOUNCE_MS = 500;

/** SFTP polling interval for remote change detection (milliseconds). */
const SFTP_POLL_INTERVAL_MS = 10_000;

// ── Types ────────────────────────────────────────────────────────

interface LocalWatcherState {
  watcher: fs.FSWatcher;
  pendingChanges: Map<string, { type: "create" | "modify" | "delete"; mtime: number }>;
  debounceTimer: ReturnType<typeof setTimeout> | null;
  propagationTimer: ReturnType<typeof setTimeout> | null;
}

interface RemoteWatcherState {
  watchId: string;
  pendingChanges: Map<string, { type: "create" | "modify" | "delete"; mtime: number }>;
  propagationTimer: ReturnType<typeof setTimeout> | null;
}

interface SftpPollingState {
  timer: ReturnType<typeof setInterval> | null;
  lastSnapshot: Map<string, { mtime: number; size: number }>;
  polling: boolean;
}

interface SyncPairState {
  pair: SyncPair;
  localWatcher: LocalWatcherState | null;
  remoteWatcher: RemoteWatcherState | null;
  sftpPoller: SftpPollingState | null;
  syncing: boolean;
  /** Tracks files currently being synced to avoid echo events. */
  syncingFiles: Set<string>;
  /** Last known mtime for files on both sides (relative path -> mtime). */
  localMtimes: Map<string, number>;
  remoteMtimes: Map<string, number>;
}

// ── WorkspaceSyncEngine ──────────────────────────────────────────

export class WorkspaceSyncEngine extends EventEmitter {
  private states = new Map<string, SyncPairState>();
  private notificationHandler: ((notification: {
    profileId: string;
    method: string;
    params: unknown;
  }) => void) | null = null;

  constructor(
    private router: RpcRouter,
    private sessionManager: SshSessionManager,
    private deltaSyncEngine: DeltaSyncEngine,
    private ftm: FileTransferManager,
    private db: BetterSqlite3.Database,
    private capabilityNegotiator?: CapabilityNegotiator,
  ) {
    super();
    this.ensureSchema();
  }

  // ── Public API ──────────────────────────────────────────────────

  /**
   * Configure a new sync pair and persist to SQLite.
   * Returns the generated sync pair ID.
   */
  addSyncPair(pair: Omit<SyncPair, "id" | "status" | "pendingChanges">): string {
    const id = crypto.randomUUID();
    const now = Date.now();

    const stmt = this.db.prepare(`
      INSERT INTO workspace_sync_pairs (id, profile_id, local_dir, remote_dir, ignore_patterns, status, created_at)
      VALUES (?, ?, ?, ?, ?, 'paused', ?)
    `);
    stmt.run(id, pair.profileId, pair.localDir, pair.remoteDir, JSON.stringify(pair.ignorePatterns), now);

    const syncPair: SyncPair = {
      id,
      profileId: pair.profileId,
      localDir: pair.localDir,
      remoteDir: pair.remoteDir,
      ignorePatterns: pair.ignorePatterns,
      status: "paused",
      lastSyncAt: pair.lastSyncAt,
      pendingChanges: 0,
    };

    this.states.set(id, {
      pair: syncPair,
      localWatcher: null,
      remoteWatcher: null,
      sftpPoller: null,
      syncing: false,
      syncingFiles: new Set(),
      localMtimes: new Map(),
      remoteMtimes: new Map(),
    });

    return id;
  }

  /**
   * Start watching and syncing for a sync pair.
   * Checks agent capabilities to decide between real-time RPC watching
   * and SFTP polling fallback. Always starts local fs.watch regardless.
   */
  async start(syncPairId: string): Promise<void> {
    const state = this.states.get(syncPairId);
    if (!state) {
      const loaded = this.loadSyncPair(syncPairId);
      if (!loaded) throw new Error(`Sync pair not found: ${syncPairId}`);
      this.states.set(syncPairId, loaded);
      return this.start(syncPairId);
    }

    if (state.pair.status === "active") return;

    // Verify local directory exists before starting watchers
    if (!fs.existsSync(state.pair.localDir)) {
      throw new Error(`Local directory does not exist: ${state.pair.localDir}`);
    }

    // Always start local file watcher regardless of remote mode
    try {
      state.localWatcher = this.startLocalWatcher(state);
    } catch (err) {
      throw new Error(
        `Failed to watch local directory "${state.pair.localDir}": ${err instanceof Error ? err.message : String(err)}`,
      );
    }

    // Decide remote watching mode based on agent capabilities
    const hasFileWatching = this.capabilityNegotiator?.isSupported(
      state.pair.profileId,
      "fileWatching",
    ) ?? false;

    if (hasFileWatching) {
      // Agent mode: real-time remote watching via RPC
      state.remoteWatcher = await this.startRemoteWatcher(state);
    } else {
      // SFTP-only mode: periodic polling via SFTP readdir + stat
      state.sftpPoller = this.startSftpPoller(state);
    }

    // Update status
    state.pair.status = "active";
    this.updateSyncPairStatus(syncPairId, "active");

    this.emitSyncStatus(state);
  }

  /**
   * Pause synchronization for a sync pair.
   * Stops all watchers and pending sync operations.
   */
  pause(syncPairId: string): void {
    const state = this.states.get(syncPairId);
    if (!state) return;

    this.stopWatchers(state);
    state.pair.status = "paused";
    this.updateSyncPairStatus(syncPairId, "paused");
    this.emitSyncStatus(state);
  }

  /**
   * Remove a sync pair entirely. Stops watchers and deletes from DB.
   */
  async removeSyncPair(syncPairId: string): Promise<void> {
    const state = this.states.get(syncPairId);
    if (state) {
      this.stopWatchers(state);
    }

    this.db.prepare("DELETE FROM sync_conflicts WHERE sync_pair_id = ?").run(syncPairId);
    this.db.prepare("DELETE FROM workspace_sync_pairs WHERE id = ?").run(syncPairId);
    this.states.delete(syncPairId);
  }

  /**
   * List all sync pairs for a profile.
   */
  listSyncPairs(profileId: string): SyncPair[] {
    const rows = this.db.prepare(
      "SELECT * FROM workspace_sync_pairs WHERE profile_id = ?",
    ).all(profileId) as Array<{
      id: string;
      profile_id: string;
      local_dir: string;
      remote_dir: string;
      ignore_patterns: string;
      status: string;
      last_sync_at: number | null;
    }>;

    return rows.map((row) => ({
      id: row.id,
      profileId: row.profile_id,
      localDir: row.local_dir,
      remoteDir: row.remote_dir,
      ignorePatterns: JSON.parse(row.ignore_patterns) as string[],
      status: row.status as SyncPair["status"],
      lastSyncAt: row.last_sync_at ?? undefined,
      pendingChanges: this.getPendingChangeCount(row.id),
    }));
  }

  /**
   * List conflicts for a sync pair.
   */
  listConflicts(syncPairId: string): SyncConflict[] {
    const rows = this.db.prepare(
      "SELECT * FROM sync_conflicts WHERE sync_pair_id = ? AND resolved_at IS NULL ORDER BY detected_at DESC",
    ).all(syncPairId) as Array<{
      id: string;
      sync_pair_id: string;
      file_path: string;
      local_mtime: number;
      remote_mtime: number;
      detected_at: number;
      resolved_at: number | null;
      resolution: string | null;
    }>;

    return rows.map((row) => ({
      id: row.id,
      syncPairId: row.sync_pair_id,
      filePath: row.file_path,
      localMtime: row.local_mtime,
      remoteMtime: row.remote_mtime,
      detectedAt: row.detected_at,
      resolvedAt: row.resolved_at ?? undefined,
      resolution: row.resolution as SyncConflict["resolution"],
    }));
  }

  /**
   * Resolve a sync conflict with the specified strategy.
   */
  resolveConflict(conflictId: string, resolution: "local" | "remote" | "both"): void {
    this.db.prepare(
      "UPDATE sync_conflicts SET resolved_at = ?, resolution = ? WHERE id = ?",
    ).run(Date.now(), resolution, conflictId);
  }

  /**
   * Stop all watchers and clean up. Called during app shutdown.
   */
  dispose(): void {
    for (const state of this.states.values()) {
      this.stopWatchers(state);
    }
    this.states.clear();
    if (this.notificationHandler) {
      this.router.removeListener("notification", this.notificationHandler);
      this.notificationHandler = null;
    }
  }

  // ── Local File Watching ─────────────────────────────────────────

  private startLocalWatcher(state: SyncPairState): LocalWatcherState {
    const watcherState: LocalWatcherState = {
      watcher: null as unknown as fs.FSWatcher,
      pendingChanges: new Map(),
      debounceTimer: null,
      propagationTimer: null,
    };

    const watcher = fs.watch(
      state.pair.localDir,
      { recursive: true, persistent: false },
      (_eventType, filename) => {
        if (!filename) return;

        // Normalize path separators
        const relativePath = filename.replace(/\\/g, "/");

        // Check ignore patterns
        if (this.shouldIgnore(relativePath, state.pair.ignorePatterns)) return;

        // Skip files currently being synced (echo prevention)
        if (state.syncingFiles.has(relativePath)) return;

        // Determine change type from filesystem
        const fullPath = path.join(state.pair.localDir, relativePath);
        let changeType: "create" | "modify" | "delete";
        let mtime = Date.now();

        try {
          const stat = fs.statSync(fullPath);
          if (stat.isDirectory()) return; // Skip directory events
          mtime = stat.mtimeMs;
          const knownMtime = state.localMtimes.get(relativePath);
          changeType = knownMtime ? "modify" : "create";
          state.localMtimes.set(relativePath, mtime);
        } catch {
          // File doesn't exist — it was deleted
          changeType = "delete";
          state.localMtimes.delete(relativePath);
        }

        watcherState.pendingChanges.set(relativePath, { type: changeType, mtime });

        // Debounce rapid events, then schedule propagation
        if (watcherState.debounceTimer) clearTimeout(watcherState.debounceTimer);
        watcherState.debounceTimer = setTimeout(() => {
          watcherState.debounceTimer = null;
          this.scheduleLocalPropagation(state, watcherState);
        }, LOCAL_DEBOUNCE_MS);
      },
    );

    watcher.on("error", () => {
      state.pair.status = "error";
      this.updateSyncPairStatus(state.pair.id, "error");
      this.emit("sync-error", { syncPairId: state.pair.id, error: "Local file watcher failed" });
    });

    watcherState.watcher = watcher;
    return watcherState;
  }

  private scheduleLocalPropagation(state: SyncPairState, watcherState: LocalWatcherState): void {
    // If already scheduled, let the existing timer fire
    if (watcherState.propagationTimer) return;

    // Schedule propagation within the 3s target (minus debounce time already spent)
    const delay = Math.max(0, LOCAL_PROPAGATION_MS - LOCAL_DEBOUNCE_MS);
    watcherState.propagationTimer = setTimeout(() => {
      watcherState.propagationTimer = null;
      this.propagateLocalChanges(state, watcherState).catch((err) => {
        this.emit("sync-error", {
          syncPairId: state.pair.id,
          error: `Local propagation failed: ${err instanceof Error ? err.message : String(err)}`,
        });
      });
    }, delay);
  }

  private async propagateLocalChanges(
    state: SyncPairState,
    watcherState: LocalWatcherState,
  ): Promise<void> {
    if (state.syncing || watcherState.pendingChanges.size === 0) return;

    // Take ownership of pending changes
    const changes = new Map(watcherState.pendingChanges);
    watcherState.pendingChanges.clear();

    state.syncing = true;
    try {
      for (const [relativePath, change] of changes) {
        // Check for conflict: was this file also modified remotely?
        const conflict = this.detectConflict(state, relativePath, change.mtime, "local");
        if (conflict) {
          this.recordConflict(state.pair.id, conflict);
          continue;
        }

        await this.syncLocalToRemote(state, relativePath, change);
      }

      state.pair.lastSyncAt = Date.now();
      this.db.prepare("UPDATE workspace_sync_pairs SET last_sync_at = ? WHERE id = ?")
        .run(state.pair.lastSyncAt, state.pair.id);
    } finally {
      state.syncing = false;
      this.emitSyncStatus(state);
    }
  }

  // ── Remote File Watching ────────────────────────────────────────

  private async startRemoteWatcher(state: SyncPairState): Promise<RemoteWatcherState> {
    const watcherState: RemoteWatcherState = {
      watchId: "",
      pendingChanges: new Map(),
      propagationTimer: null,
    };

    try {
      const result = await this.router.sendToRemoteAgent(state.pair.profileId, "watch_start", {
        path: state.pair.remoteDir,
        recursive: true,
        ignore_patterns: state.pair.ignorePatterns,
      }) as { watch_id: string };

      watcherState.watchId = result.watch_id;
    } catch (err) {
      this.emit("sync-error", {
        syncPairId: state.pair.id,
        error: `Failed to start remote watcher: ${err instanceof Error ? err.message : String(err)}`,
      });
      return watcherState;
    }

    // Listen for remote file system events via RPC notifications
    if (!this.notificationHandler) {
      this.notificationHandler = (notification) => {
        if (notification.method === "fs_event") {
          this.handleRemoteFsEvent(notification.params);
        }
      };
      this.router.on("notification", this.notificationHandler);
    }

    return watcherState;
  }

  private handleRemoteFsEvent(params: unknown): void {
    const event = params as {
      watch_id: string;
      events: Array<{ path: string; kind: "create" | "modify" | "delete" | "rename"; timestamp: number }>;
    };

    // Find the sync pair state that owns this watch_id
    let targetState: SyncPairState | null = null;
    for (const state of this.states.values()) {
      if (state.remoteWatcher?.watchId === event.watch_id) {
        targetState = state;
        break;
      }
    }

    if (!targetState || targetState.pair.status !== "active") return;

    const remoteWatcher = targetState.remoteWatcher;
    if (!remoteWatcher) return;

    for (const fsEvent of event.events) {
      // Convert absolute remote path to relative
      const relativePath = this.toRelativePath(fsEvent.path, targetState.pair.remoteDir);
      if (!relativePath) continue;

      // Check ignore patterns
      if (this.shouldIgnore(relativePath, targetState.pair.ignorePatterns)) continue;

      // Skip files currently being synced (echo prevention)
      if (targetState.syncingFiles.has(relativePath)) continue;

      const changeType = fsEvent.kind === "rename" ? "modify" : fsEvent.kind;
      remoteWatcher.pendingChanges.set(relativePath, {
        type: changeType as "create" | "modify" | "delete",
        mtime: fsEvent.timestamp * 1000, // Convert seconds to ms if needed
      });

      // Update remote mtime tracking
      if (changeType !== "delete") {
        targetState.remoteMtimes.set(relativePath, fsEvent.timestamp * 1000);
      } else {
        targetState.remoteMtimes.delete(relativePath);
      }
    }

    // Schedule remote propagation within 5s target
    this.scheduleRemotePropagation(targetState, remoteWatcher);
  }

  private scheduleRemotePropagation(state: SyncPairState, watcherState: RemoteWatcherState): void {
    if (watcherState.propagationTimer) return;

    watcherState.propagationTimer = setTimeout(() => {
      watcherState.propagationTimer = null;
      this.propagateRemoteChanges(state, watcherState).catch((err) => {
        this.emit("sync-error", {
          syncPairId: state.pair.id,
          error: `Remote propagation failed: ${err instanceof Error ? err.message : String(err)}`,
        });
      });
    }, REMOTE_PROPAGATION_MS);
  }

  // ── SFTP Polling Fallback ───────────────────────────────────────

  /**
   * Start an SFTP-based polling watcher as a fallback when the remote
   * agent is unavailable. Polls the remote directory tree every 10s,
   * comparing file mtimes against a stored snapshot to detect changes
   * and deletions.
   */
  startSftpPoller(state: SyncPairState): SftpPollingState {
    const pollerState: SftpPollingState = {
      timer: null,
      lastSnapshot: new Map(),
      polling: false,
    };

    pollerState.timer = setInterval(() => {
      if (pollerState.polling || state.pair.status !== "active") return;
      this.pollRemoteDirectory(state, pollerState).catch((err) => {
        this.emit("sync-error", {
          syncPairId: state.pair.id,
          error: `SFTP poll failed: ${err instanceof Error ? err.message : String(err)}`,
        });
      });
    }, SFTP_POLL_INTERVAL_MS);

    return pollerState;
  }

  /**
   * Execute a single poll cycle: recursively scan the remote directory,
   * compare against the last snapshot, and queue changed/deleted files.
   */
  private async pollRemoteDirectory(
    state: SyncPairState,
    pollerState: SftpPollingState,
  ): Promise<void> {
    pollerState.polling = true;
    try {
      const sftp = this.getSftpForProfile(state.pair.profileId);
      if (!sftp) return;

      const currentSnapshot = new Map<string, { mtime: number; size: number }>();
      await this.scanRemoteDir(sftp, state.pair.remoteDir, "", state.pair.ignorePatterns, currentSnapshot);

      // First poll: just store the snapshot without triggering changes
      if (pollerState.lastSnapshot.size === 0) {
        pollerState.lastSnapshot = currentSnapshot;
        // Seed remoteMtimes from the initial snapshot
        for (const [relativePath, entry] of currentSnapshot) {
          state.remoteMtimes.set(relativePath, entry.mtime);
        }
        return;
      }

      // Detect changes: files with newer mtime than snapshot
      const changes = new Map<string, { type: "create" | "modify" | "delete"; mtime: number }>();

      for (const [relativePath, current] of currentSnapshot) {
        const previous = pollerState.lastSnapshot.get(relativePath);
        if (!previous) {
          // New file
          changes.set(relativePath, { type: "create", mtime: current.mtime });
          state.remoteMtimes.set(relativePath, current.mtime);
        } else if (current.mtime > previous.mtime) {
          // Modified file
          changes.set(relativePath, { type: "modify", mtime: current.mtime });
          state.remoteMtimes.set(relativePath, current.mtime);
        }
      }

      // Detect deletions: files in snapshot but missing from current scan
      for (const [relativePath] of pollerState.lastSnapshot) {
        if (!currentSnapshot.has(relativePath)) {
          changes.set(relativePath, { type: "delete", mtime: Date.now() });
          state.remoteMtimes.delete(relativePath);
        }
      }

      // Update snapshot
      pollerState.lastSnapshot = currentSnapshot;

      // Propagate detected changes through the same path as the agent watcher
      if (changes.size > 0) {
        await this.propagatePolledChanges(state, changes);
      }
    } finally {
      pollerState.polling = false;
    }
  }

  /**
   * Recursively scan a remote directory via SFTP readdir + stat.
   */
  private async scanRemoteDir(
    sftp: import("ssh2").SFTPWrapper,
    baseDir: string,
    relativePath: string,
    ignorePatterns: string[],
    snapshot: Map<string, { mtime: number; size: number }>,
  ): Promise<void> {
    const fullPath = relativePath ? `${baseDir}/${relativePath}` : baseDir;

    const entries = await new Promise<Array<{ filename: string; attrs: { mode: number; mtime: number; size: number } }>>(
      (resolve, reject) => {
        sftp.readdir(fullPath, (err, list) => {
          if (err) {
            reject(err);
            return;
          }
          resolve(list as any);
        });
      },
    );

    for (const entry of entries) {
      const entryRelative = relativePath
        ? `${relativePath}/${entry.filename}`
        : entry.filename;

      if (this.shouldIgnore(entryRelative, ignorePatterns)) continue;

      // Check if directory (mode & 0o40000)
      const isDir = (entry.attrs.mode & 0o40000) !== 0;

      if (isDir) {
        await this.scanRemoteDir(sftp, baseDir, entryRelative, ignorePatterns, snapshot);
      } else {
        snapshot.set(entryRelative, {
          mtime: entry.attrs.mtime * 1000, // Convert seconds to ms
          size: entry.attrs.size,
        });
      }
    }
  }

  /**
   * Propagate changes detected by SFTP polling through the same
   * download path used by the agent-based remote watcher.
   */
  private async propagatePolledChanges(
    state: SyncPairState,
    changes: Map<string, { type: "create" | "modify" | "delete"; mtime: number }>,
  ): Promise<void> {
    if (state.syncing) return;

    state.syncing = true;
    try {
      for (const [relativePath, change] of changes) {
        // Skip files currently being synced (echo prevention)
        if (state.syncingFiles.has(relativePath)) continue;

        // Check for conflict
        const conflict = this.detectConflict(state, relativePath, change.mtime, "remote");
        if (conflict) {
          this.recordConflict(state.pair.id, conflict);
          continue;
        }

        await this.syncRemoteToLocal(state, relativePath, change);
      }

      state.pair.lastSyncAt = Date.now();
      this.db.prepare("UPDATE workspace_sync_pairs SET last_sync_at = ? WHERE id = ?")
        .run(state.pair.lastSyncAt, state.pair.id);
    } finally {
      state.syncing = false;
      this.emitSyncStatus(state);
    }
  }

  /**
   * Get the SFTP wrapper for a profile from the session manager.
   */
  private getSftpForProfile(profileId: string): import("ssh2").SFTPWrapper | undefined {
    const conn = this.sessionManager.getConnection(profileId);
    return conn?.channels.sftp;
  }

  private async propagateRemoteChanges(
    state: SyncPairState,
    watcherState: RemoteWatcherState,
  ): Promise<void> {
    if (state.syncing || watcherState.pendingChanges.size === 0) return;

    const changes = new Map(watcherState.pendingChanges);
    watcherState.pendingChanges.clear();

    state.syncing = true;
    try {
      for (const [relativePath, change] of changes) {
        // Check for conflict
        const conflict = this.detectConflict(state, relativePath, change.mtime, "remote");
        if (conflict) {
          this.recordConflict(state.pair.id, conflict);
          continue;
        }

        await this.syncRemoteToLocal(state, relativePath, change);
      }

      state.pair.lastSyncAt = Date.now();
      this.db.prepare("UPDATE workspace_sync_pairs SET last_sync_at = ? WHERE id = ?")
        .run(state.pair.lastSyncAt, state.pair.id);
    } finally {
      state.syncing = false;
      this.emitSyncStatus(state);
    }
  }

  // ── Sync Operations ─────────────────────────────────────────────

  private async syncLocalToRemote(
    state: SyncPairState,
    relativePath: string,
    change: { type: "create" | "modify" | "delete"; mtime: number },
  ): Promise<void> {
    const localPath = path.join(state.pair.localDir, relativePath);
    const remotePath = `${state.pair.remoteDir}/${relativePath}`;

    state.syncingFiles.add(relativePath);
    try {
      if (change.type === "delete") {
        await this.retryWithBackoff(() =>
          this.router.sendToRemoteAgent(state.pair.profileId, "file_delete", { path: remotePath }),
        );
        state.remoteMtimes.delete(relativePath);
      } else {
        // Check file size for delta sync decision
        let fileSize = 0;
        try {
          const stat = fs.statSync(localPath);
          fileSize = stat.size;
        } catch {
          return; // File disappeared between event and sync
        }

        if (fileSize >= DELTA_SYNC_THRESHOLD) {
          // Use delta sync for large files
          await this.retryWithBackoff(async () => {
            const plan = await this.deltaSyncEngine.computeDelta(
              state.pair.profileId,
              localPath,
              remotePath,
            );
            if (plan.fallbackToFull) {
              this.ftm.upload(state.pair.profileId, localPath, remotePath);
            } else {
              await this.deltaSyncEngine.applyDeltaUpload(
                state.pair.profileId,
                plan,
                localPath,
                remotePath,
              );
            }
          });
        } else {
          // Full file transfer for small files
          await this.retryWithBackoff(async () => {
            this.ftm.upload(state.pair.profileId, localPath, remotePath);
          });
        }

        state.remoteMtimes.set(relativePath, change.mtime);
      }
    } finally {
      // Remove from syncing set after a short delay to avoid echo events
      setTimeout(() => state.syncingFiles.delete(relativePath), 1000);
    }
  }

  private async syncRemoteToLocal(
    state: SyncPairState,
    relativePath: string,
    change: { type: "create" | "modify" | "delete"; mtime: number },
  ): Promise<void> {
    const localPath = path.join(state.pair.localDir, relativePath);
    const remotePath = `${state.pair.remoteDir}/${relativePath}`;

    state.syncingFiles.add(relativePath);
    try {
      if (change.type === "delete") {
        try {
          fs.unlinkSync(localPath);
        } catch {
          // File already gone locally — fine
        }
        state.localMtimes.delete(relativePath);
      } else {
        // Ensure local directory exists
        const dir = path.dirname(localPath);
        fs.mkdirSync(dir, { recursive: true });

        // Check remote file size for delta sync decision
        let fileSize = 0;
        try {
          const stat = await this.router.sendToRemoteAgent(
            state.pair.profileId,
            "file_stat",
            { path: remotePath },
          ) as { size: number; mtime: number; exists: boolean };
          fileSize = stat.size;
        } catch {
          fileSize = 0;
        }

        if (fileSize >= DELTA_SYNC_THRESHOLD) {
          await this.retryWithBackoff(async () => {
            const plan = await this.deltaSyncEngine.computeDelta(
              state.pair.profileId,
              localPath,
              remotePath,
            );
            if (plan.fallbackToFull) {
              this.ftm.download(state.pair.profileId, remotePath, localPath);
            } else {
              await this.deltaSyncEngine.applyDeltaDownload(
                state.pair.profileId,
                plan,
                localPath,
                remotePath,
              );
            }
          });
        } else {
          await this.retryWithBackoff(async () => {
            this.ftm.download(state.pair.profileId, remotePath, localPath);
          });
        }

        state.localMtimes.set(relativePath, change.mtime);
      }
    } finally {
      setTimeout(() => state.syncingFiles.delete(relativePath), 1000);
    }
  }

  // ── Conflict Detection ──────────────────────────────────────────

  /**
   * Detect if a file change creates a conflict. A conflict occurs when
   * the same file has been modified on both sides since the last sync.
   */
  private detectConflict(
    state: SyncPairState,
    relativePath: string,
    changeMtime: number,
    source: "local" | "remote",
  ): SyncConflict | null {
    const localMtime = state.localMtimes.get(relativePath);
    const remoteMtime = state.remoteMtimes.get(relativePath);

    if (!localMtime || !remoteMtime) return null;

    const lastSync = state.pair.lastSyncAt ?? 0;

    // Conflict: both sides modified since last sync
    if (source === "local" && remoteMtime > lastSync && changeMtime > lastSync) {
      return {
        id: crypto.randomUUID(),
        syncPairId: state.pair.id,
        filePath: relativePath,
        localMtime: changeMtime,
        remoteMtime,
        detectedAt: Date.now(),
      };
    }

    if (source === "remote" && localMtime > lastSync && changeMtime > lastSync) {
      return {
        id: crypto.randomUUID(),
        syncPairId: state.pair.id,
        filePath: relativePath,
        localMtime,
        remoteMtime: changeMtime,
        detectedAt: Date.now(),
      };
    }

    return null;
  }

  private recordConflict(syncPairId: string, conflict: SyncConflict): void {
    this.db.prepare(`
      INSERT INTO sync_conflicts (id, sync_pair_id, file_path, local_mtime, remote_mtime, detected_at)
      VALUES (?, ?, ?, ?, ?, ?)
    `).run(
      conflict.id,
      syncPairId,
      conflict.filePath,
      conflict.localMtime,
      conflict.remoteMtime,
      conflict.detectedAt,
    );

    this.emit("sync-conflict", conflict);
  }

  // ── Retry Logic ────────────────────────────────────────────────

  /**
   * Retry an async operation with exponential backoff.
   * Delays: 1s, 2s, 4s (base * 2^attempt).
   */
  private async retryWithBackoff(operation: () => Promise<unknown>): Promise<void> {
    let lastError: Error | null = null;

    for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
      try {
        await operation();
        return;
      } catch (err) {
        lastError = err instanceof Error ? err : new Error(String(err));

        if (attempt < MAX_RETRIES - 1) {
          const delay = BASE_RETRY_DELAY_MS * Math.pow(2, attempt);
          await this.sleep(delay);
        }
      }
    }

    if (lastError) {
      this.emit("sync-error", {
        syncPairId: "unknown",
        error: `Operation failed after ${MAX_RETRIES} attempts: ${lastError.message}`,
      });
      throw lastError;
    }
  }

  // ── Helpers ────────────────────────────────────────────────────

  /**
   * Check if a relative path matches any of the configured ignore patterns.
   */
  private shouldIgnore(relativePath: string, patterns: string[]): boolean {
    for (const pattern of patterns) {
      if (minimatch(relativePath, pattern, { dot: true })) return true;
      // Also check if any path segment matches (for patterns like "node_modules")
      const segments = relativePath.split("/");
      for (const segment of segments) {
        if (minimatch(segment, pattern, { dot: true })) return true;
      }
    }
    return false;
  }

  /**
   * Convert an absolute remote path to a path relative to the sync pair's remote dir.
   */
  private toRelativePath(absolutePath: string, baseDir: string): string | null {
    const normalized = absolutePath.replace(/\\/g, "/");
    const normalizedBase = baseDir.replace(/\\/g, "/").replace(/\/$/, "");

    if (!normalized.startsWith(normalizedBase + "/")) return null;
    return normalized.slice(normalizedBase.length + 1);
  }

  private stopWatchers(state: SyncPairState): void {
    // Stop local watcher
    if (state.localWatcher) {
      if (state.localWatcher.debounceTimer) clearTimeout(state.localWatcher.debounceTimer);
      if (state.localWatcher.propagationTimer) clearTimeout(state.localWatcher.propagationTimer);
      try {
        state.localWatcher.watcher.close();
      } catch {
        // Already closed
      }
      state.localWatcher = null;
    }

    // Stop remote watcher
    if (state.remoteWatcher) {
      if (state.remoteWatcher.propagationTimer) clearTimeout(state.remoteWatcher.propagationTimer);
      if (state.remoteWatcher.watchId) {
        this.router.sendToRemoteAgent(state.pair.profileId, "watch_stop", {
          watch_id: state.remoteWatcher.watchId,
        }).catch(() => {
          // Best-effort cleanup — connection may already be gone
        });
      }
      state.remoteWatcher = null;
    }

    // Stop SFTP poller
    if (state.sftpPoller) {
      if (state.sftpPoller.timer) clearInterval(state.sftpPoller.timer);
      state.sftpPoller = null;
    }
  }

  private updateSyncPairStatus(syncPairId: string, status: SyncPair["status"]): void {
    this.db.prepare("UPDATE workspace_sync_pairs SET status = ? WHERE id = ?")
      .run(status, syncPairId);
  }

  private emitSyncStatus(state: SyncPairState): void {
    const pendingChanges = this.getPendingChangeCount(state.pair.id);
    state.pair.pendingChanges = pendingChanges;

    this.emit("sync-status", {
      syncPairId: state.pair.id,
      pendingChanges,
      lastSyncAt: state.pair.lastSyncAt ?? 0,
      status: state.pair.status,
    });
  }

  private getPendingChangeCount(syncPairId: string): number {
    const state = this.states.get(syncPairId);
    if (!state) return 0;

    const localPending = state.localWatcher?.pendingChanges.size ?? 0;
    const remotePending = state.remoteWatcher?.pendingChanges.size ?? 0;
    return localPending + remotePending;
  }

  private loadSyncPair(syncPairId: string): SyncPairState | null {
    const row = this.db.prepare("SELECT * FROM workspace_sync_pairs WHERE id = ?")
      .get(syncPairId) as {
        id: string;
        profile_id: string;
        local_dir: string;
        remote_dir: string;
        ignore_patterns: string;
        status: string;
        last_sync_at: number | null;
      } | undefined;

    if (!row) return null;

    const pair: SyncPair = {
      id: row.id,
      profileId: row.profile_id,
      localDir: row.local_dir,
      remoteDir: row.remote_dir,
      ignorePatterns: JSON.parse(row.ignore_patterns) as string[],
      status: row.status as SyncPair["status"],
      lastSyncAt: row.last_sync_at ?? undefined,
      pendingChanges: 0,
    };

    return {
      pair,
      localWatcher: null,
      remoteWatcher: null,
      sftpPoller: null,
      syncing: false,
      syncingFiles: new Set(),
      localMtimes: new Map(),
      remoteMtimes: new Map(),
    };
  }

  private ensureSchema(): void {
    // Tables are created by the migration (task 1.1), but ensure they exist
    // for safety in case this module is used standalone in tests.
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS workspace_sync_pairs (
        id TEXT PRIMARY KEY,
        profile_id TEXT NOT NULL,
        local_dir TEXT NOT NULL,
        remote_dir TEXT NOT NULL,
        ignore_patterns TEXT NOT NULL DEFAULT '[]',
        status TEXT NOT NULL DEFAULT 'paused' CHECK (status IN ('active', 'paused', 'error')),
        last_sync_at INTEGER,
        created_at INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS sync_conflicts (
        id TEXT PRIMARY KEY,
        sync_pair_id TEXT NOT NULL,
        file_path TEXT NOT NULL,
        local_mtime INTEGER NOT NULL,
        remote_mtime INTEGER NOT NULL,
        detected_at INTEGER NOT NULL,
        resolved_at INTEGER,
        resolution TEXT CHECK (resolution IN ('local', 'remote', 'both')),
        FOREIGN KEY (sync_pair_id) REFERENCES workspace_sync_pairs(id) ON DELETE CASCADE
      );
    `);
  }

  private sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}
