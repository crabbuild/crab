/**
 * Remote Workspace Manager — manages remote workspaces as IDE-like
 * environments with directory tree indexing, caching, and offline
 * resilience for unsaved edits.
 *
 * Indexes remote directory trees (up to 50,000 entries within 5s),
 * caches them locally, invalidates on agent file change events, and
 * persists unsaved edits to local storage for offline resilience.
 *
 * Reuses the existing RemoteFileEditor for file open/edit/save
 * operations.
 */

import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as path from "node:path";
import { randomUUID } from "node:crypto";
import { app } from "electron";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteFileEditor, RemoteFileContent, WriteResult } from "./ssh-file-editor";
import type { RemoteDirEntry } from "./ssh-file-browser";

// ── Constants ────────────────────────────────────────────────────

/** Maximum entries the directory tree indexer will handle. */
const MAX_TREE_ENTRIES = 50_000;

/** Directory tree indexing timeout (milliseconds). */
const INDEX_TIMEOUT_MS = 5_000;

/** Cache staleness threshold (milliseconds). Default 60 seconds. */
const CACHE_STALE_MS = 60_000;

/** Directory for persisting unsaved edits. */
const UNSAVED_EDITS_DIR = "ssh-unsaved-edits";

/** Maximum search results before capping. */
const SEARCH_MAX_RESULTS = 10_000;

/** Search timeout (milliseconds). */
const SEARCH_TIMEOUT_MS = 60_000;

/** Grace period before escalating SIGTERM to SIGKILL (milliseconds). */
const KILL_GRACE_MS = 5_000;

// ── Types ────────────────────────────────────────────────────────

export interface WorkspaceInfo {
  profileId: string;
  remotePath: string;
  tree: RemoteDirEntry[];
  totalEntries: number;
  indexedAt: number;
  watchMode: "inotify" | "fsevents" | "polling" | "none";
  connected: boolean;
}

interface CachedTree {
  entries: RemoteDirEntry[];
  indexedAt: number;
  totalEntries: number;
}

interface UnsavedEdit {
  profileId: string;
  filePath: string;
  content: string;
  savedAt: number;
}

interface WorkspaceState {
  profileId: string;
  remotePath: string;
  cache: CachedTree | null;
  watchMode: "inotify" | "fsevents" | "polling" | "none";
  connected: boolean;
}

export interface WorkspaceSearchOptions {
  /** Restrict search to a sub-path within the workspace. */
  path?: string;
  /** Treat query as a regular expression. */
  regex?: boolean;
  /** Case-sensitive matching. */
  caseSensitive?: boolean;
  /** Maximum results to return (capped at 10,000). */
  maxResults?: number;
}

// ── RemoteWorkspaceManager ───────────────────────────────────────

export class RemoteWorkspaceManager extends EventEmitter {
  private workspaces = new Map<string, WorkspaceState>();
  private notificationHandler: ((notification: {
    profileId: string;
    method: string;
    params: unknown;
  }) => void) | null = null;

  private readonly unsavedEditsBasePath: string;
  /** Active search streams mapped to their timeout timers. */
  private activeStreams = new Map<string, { timer: ReturnType<typeof setTimeout>; profileId: string; matchCount: number }>();
  /** Active exec command streams mapped to their state. */
  private execStreams = new Map<string, { profileId: string; killTimer?: ReturnType<typeof setTimeout> }>();
  /** Active tail streams mapped to their profile and file path. */
  private tailStreams = new Map<string, { profileId: string; filePath: string }>();

  constructor(
    private readonly router: RpcRouter,
    private readonly sessionManager: SshSessionManager,
    private readonly fileEditor: RemoteFileEditor,
  ) {
    super();
    this.unsavedEditsBasePath = path.join(
      app.getPath("userData"),
      UNSAVED_EDITS_DIR,
    );
    this.setupNotificationListener();
  }

  // ── Public API ──────────────────────────────────────────────────

  /**
   * Open a remote directory as a workspace. Indexes the directory tree
   * via agent RPC, caches locally, and returns workspace info.
   *
   * Handles up to 50,000 entries within 5 seconds.
   */
  async openWorkspace(profileId: string, remotePath: string): Promise<WorkspaceInfo> {
    this.assertConnected(profileId);

    // Index the directory tree via agent RPC
    const { entries, totalEntries, watchMode } = await this.indexDirectoryTree(
      profileId,
      remotePath,
    );

    const now = Date.now();
    const cache: CachedTree = {
      entries,
      indexedAt: now,
      totalEntries,
    };

    const state: WorkspaceState = {
      profileId,
      remotePath,
      cache,
      watchMode,
      connected: true,
    };

    this.workspaces.set(profileId, state);

    const info: WorkspaceInfo = {
      profileId,
      remotePath,
      tree: entries,
      totalEntries,
      indexedAt: now,
      watchMode,
      connected: true,
    };

    this.emit("workspace-opened", info);
    return info;
  }

  /**
   * Close a workspace. Cleans up watchers and caches for the profile.
   */
  closeWorkspace(profileId: string): void {
    const state = this.workspaces.get(profileId);
    if (!state) return;

    // Clear cached tree
    state.cache = null;
    this.workspaces.delete(profileId);

    this.emit("workspace-closed", { profileId });
  }

  /**
   * Get the cached directory tree for a workspace. Refreshes if the
   * cache is stale (older than CACHE_STALE_MS).
   */
  async getDirectoryTree(profileId: string): Promise<RemoteDirEntry[]> {
    const state = this.workspaces.get(profileId);
    if (!state) {
      throw new Error(`No workspace open for profile: ${profileId}`);
    }

    // Return cached tree if fresh
    if (state.cache && !this.isCacheStale(state.cache)) {
      return state.cache.entries;
    }

    // Refresh the cache if connected
    if (state.connected) {
      try {
        const { entries, totalEntries } = await this.indexDirectoryTree(
          profileId,
          state.remotePath,
        );

        state.cache = {
          entries,
          indexedAt: Date.now(),
          totalEntries,
        };

        return entries;
      } catch {
        // If refresh fails but we have stale cache, return it
        if (state.cache) {
          return state.cache.entries;
        }
        throw new Error(`Failed to refresh directory tree for profile: ${profileId}`);
      }
    }

    // Disconnected — return stale cache if available
    if (state.cache) {
      return state.cache.entries;
    }

    return [];
  }

  /**
   * Invalidate specific paths in the cached directory tree.
   * Called when the agent reports file system changes.
   */
  invalidateCache(profileId: string, changedPaths: string[]): void {
    const state = this.workspaces.get(profileId);
    if (!state || !state.cache) return;

    const changedSet = new Set(changedPaths);
    const changedDirs = new Set<string>();

    // Collect parent directories of changed paths
    for (const changedPath of changedPaths) {
      const dir = this.getParentPath(changedPath);
      if (dir) changedDirs.add(dir);
    }

    // Remove entries that match changed paths
    state.cache.entries = state.cache.entries.filter(
      (entry) => !changedSet.has(entry.path),
    );

    // Mark cache as needing refresh by setting indexedAt to 0
    // so the next getDirectoryTree call will refresh
    state.cache.indexedAt = 0;

    this.emit("cache-invalidated", { profileId, changedPaths });
  }

  /**
   * Persist unsaved edits to local storage for offline resilience.
   * Ensures no edits are lost if the connection drops.
   */
  persistUnsavedEdits(profileId: string, filePath: string, content: string): void {
    const edit: UnsavedEdit = {
      profileId,
      filePath,
      content,
      savedAt: Date.now(),
    };

    const editDir = this.getUnsavedEditsDir(profileId);
    fs.mkdirSync(editDir, { recursive: true });

    // Use a safe filename derived from the remote path
    const safeFileName = this.toSafeFileName(filePath);
    const editPath = path.join(editDir, `${safeFileName}.json`);

    fs.writeFileSync(editPath, JSON.stringify(edit, null, 2), "utf-8");
  }

  /**
   * Recover any persisted unsaved edits for a profile.
   * Returns all unsaved edits that were persisted for offline resilience.
   */
  recoverUnsavedEdits(profileId: string): UnsavedEdit[] {
    const editDir = this.getUnsavedEditsDir(profileId);

    if (!fs.existsSync(editDir)) {
      return [];
    }

    const edits: UnsavedEdit[] = [];

    try {
      const files = fs.readdirSync(editDir);
      for (const file of files) {
        if (!file.endsWith(".json")) continue;

        try {
          const filePath = path.join(editDir, file);
          const raw = fs.readFileSync(filePath, "utf-8");
          const edit = JSON.parse(raw) as UnsavedEdit;
          edits.push(edit);
        } catch {
          // Skip corrupted edit files
        }
      }
    } catch {
      // Directory read failed — return empty
    }

    return edits;
  }

  /**
   * Remove a persisted unsaved edit after it has been saved successfully.
   */
  clearUnsavedEdit(profileId: string, filePath: string): void {
    const editDir = this.getUnsavedEditsDir(profileId);
    const safeFileName = this.toSafeFileName(filePath);
    const editPath = path.join(editDir, `${safeFileName}.json`);

    try {
      if (fs.existsSync(editPath)) {
        fs.unlinkSync(editPath);
      }
    } catch {
      // Best-effort cleanup
    }
  }

  /**
   * Open a remote file for editing. Delegates to RemoteFileEditor.
   */
  async openFile(profileId: string, filePath: string): Promise<RemoteFileContent> {
    return this.fileEditor.readFile(profileId, filePath);
  }

  /**
   * Save a remote file. Delegates to RemoteFileEditor.
   * On success, clears any persisted unsaved edit for this file.
   */
  async saveFile(
    profileId: string,
    filePath: string,
    content: string,
    expectedMtime?: number,
  ): Promise<WriteResult> {
    const result = await this.fileEditor.writeFile(profileId, filePath, content, expectedMtime);

    if (result.success) {
      this.clearUnsavedEdit(profileId, filePath);
    }

    return result;
  }

  /**
   * Create a new file in the remote workspace. Delegates to RemoteFileEditor.
   */
  async createFile(profileId: string, filePath: string): Promise<void> {
    return this.fileEditor.createFile(profileId, filePath);
  }

  /**
   * Delete a file in the remote workspace. Delegates to RemoteFileEditor.
   */
  async deleteFile(profileId: string, filePath: string): Promise<void> {
    return this.fileEditor.deleteFile(profileId, filePath);
  }

  /**
   * Rename a file in the remote workspace. Delegates to RemoteFileEditor.
   */
  async renameFile(profileId: string, oldPath: string, newPath: string): Promise<void> {
    return this.fileEditor.renameFile(profileId, oldPath, newPath);
  }

  /**
   * Mark a workspace as disconnected. Retains cached tree and unsaved edits.
   */
  markDisconnected(profileId: string): void {
    const state = this.workspaces.get(profileId);
    if (!state) return;

    state.connected = false;
    this.emit("workspace-disconnected", { profileId });
  }

  /**
   * Mark a workspace as reconnected. Triggers cache refresh.
   */
  async markReconnected(profileId: string): Promise<void> {
    const state = this.workspaces.get(profileId);
    if (!state) return;

    state.connected = true;

    // Refresh the directory tree on reconnect
    try {
      const { entries, totalEntries } = await this.indexDirectoryTree(
        profileId,
        state.remotePath,
      );

      state.cache = {
        entries,
        indexedAt: Date.now(),
        totalEntries,
      };

      this.emit("workspace-reconnected", { profileId, tree: entries });
    } catch {
      // Reconnect succeeded but tree refresh failed — keep stale cache
      this.emit("workspace-reconnected", {
        profileId,
        tree: state.cache?.entries ?? [],
      });
    }
  }

  /**
   * Check if a workspace is open for a profile.
   */
  isWorkspaceOpen(profileId: string): boolean {
    return this.workspaces.has(profileId);
  }

  /**
   * Get workspace state for a profile (if open).
   */
  getWorkspaceState(profileId: string): WorkspaceInfo | null {
    const state = this.workspaces.get(profileId);
    if (!state) return null;

    return {
      profileId: state.profileId,
      remotePath: state.remotePath,
      tree: state.cache?.entries ?? [],
      totalEntries: state.cache?.totalEntries ?? 0,
      indexedAt: state.cache?.indexedAt ?? 0,
      watchMode: state.watchMode,
      connected: state.connected,
    };
  }

  /**
   * Clean up all workspaces. Called during app shutdown.
   */
  dispose(): void {
    for (const profileId of this.workspaces.keys()) {
      this.closeWorkspace(profileId);
    }

    // Cancel all active search streams
    for (const [streamId, stream] of this.activeStreams) {
      clearTimeout(stream.timer);
      this.emit("stream-end", { streamId, error: "Operation cancelled: shutdown" });
    }
    this.activeStreams.clear();

    // Cancel all active exec streams
    for (const [streamId, stream] of this.execStreams) {
      if (stream.killTimer) clearTimeout(stream.killTimer);
      this.emit("stream-end", { streamId, error: "Operation cancelled: shutdown" });
    }
    this.execStreams.clear();

    // Cancel all active tail streams
    for (const [streamId] of this.tailStreams) {
      this.emit("stream-end", { streamId, error: "Operation cancelled: shutdown" });
    }
    this.tailStreams.clear();

    if (this.notificationHandler) {
      this.router.removeListener("notification", this.notificationHandler);
      this.notificationHandler = null;
    }
  }

  // ── Search ──────────────────────────────────────────────────────

  /**
   * Search the remote workspace for text matches. Calls the agent's
   * "workspace_search" RPC and streams results back via "stream-data"
   * events keyed by the returned stream ID.
   *
   * Results are capped at 10,000 matches or a 60-second timeout,
   * whichever comes first. Emits "stream-end" when the search
   * completes or is capped.
   */
  searchWorkspace(
    profileId: string,
    query: string,
    opts?: WorkspaceSearchOptions,
  ): string {
    this.assertConnected(profileId);

    const state = this.workspaces.get(profileId);
    if (!state) {
      throw new Error(`No workspace open for profile: ${profileId}`);
    }

    const streamId = randomUUID();
    const maxResults = Math.min(opts?.maxResults ?? SEARCH_MAX_RESULTS, SEARCH_MAX_RESULTS);

    // Set up the 60-second timeout
    const timer = setTimeout(() => {
      const stream = this.activeStreams.get(streamId);
      if (!stream) return;
      this.activeStreams.delete(streamId);
      this.emit("stream-end", {
        streamId,
        error: "Search timed out after 60 seconds",
      });
    }, SEARCH_TIMEOUT_MS);

    this.activeStreams.set(streamId, { timer, profileId, matchCount: 0 });

    // Build RPC params
    const params: Record<string, unknown> = {
      path: opts?.path ?? state.remotePath,
      query,
      stream_id: streamId,
      max_results: maxResults,
    };

    if (opts?.regex !== undefined) {
      params.regex = opts.regex;
    }
    if (opts?.caseSensitive !== undefined) {
      params.case_sensitive = opts.caseSensitive;
    }

    // Fire the RPC call asynchronously — results arrive via notifications
    this.router
      .sendToRemoteAgent(profileId, "workspace_search", params)
      .then((result) => {
        const stream = this.activeStreams.get(streamId);
        if (!stream) {
          // Already timed out or capped
          return;
        }
        clearTimeout(stream.timer);
        this.activeStreams.delete(streamId);

        const response = result as { exit_code?: number; error?: string; total_matches?: number };
        this.emit("stream-end", {
          streamId,
          exitCode: response.exit_code ?? 0,
          error: response.error,
        });
      })
      .catch((err) => {
        const stream = this.activeStreams.get(streamId);
        if (!stream) {
          // Already timed out or capped
          return;
        }
        clearTimeout(stream.timer);
        this.activeStreams.delete(streamId);

        const message = err instanceof Error ? err.message : String(err);
        this.emit("stream-end", {
          streamId,
          error: message,
        });
      });

    return streamId;
  }

  /**
   * Cancel an active search stream.
   */
  cancelSearch(streamId: string): void {
    const stream = this.activeStreams.get(streamId);
    if (stream) {
      clearTimeout(stream.timer);
      this.activeStreams.delete(streamId);
      this.emit("stream-end", {
        streamId,
        error: "Search cancelled",
      });
    }
  }

  /**
   * Check whether a search stream is currently active.
   */
  isSearchActive(streamId: string): boolean {
    return this.activeStreams.has(streamId);
  }

  // ── Log Tailing ─────────────────────────────────────────────────

  /**
   * Start tailing a remote file. Calls the agent's "tail_start" RPC
   * and returns a stream ID for the renderer to subscribe to new lines.
   *
   * New lines are delivered within 1 second of being written on the
   * remote via "stream-data" events keyed by the returned stream ID.
   * When tailing is stopped, a "stream-end" event is emitted.
   */
  async tailFile(profileId: string, filePath: string): Promise<string> {
    this.assertConnected(profileId);

    const streamId = randomUUID();

    // Call the agent's tail_start RPC
    try {
      await this.router.sendToRemoteAgent(profileId, "tail_start", {
        path: filePath,
        stream_id: streamId,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      throw new Error(`Failed to start tailing "${filePath}": ${message}`);
    }

    this.tailStreams.set(streamId, { profileId, filePath });

    return streamId;
  }

  /**
   * Stop tailing a remote file. Calls the agent's "tail_stop" RPC
   * and emits a "stream-end" event for the stream.
   */
  async stopTail(streamId: string): Promise<void> {
    const stream = this.tailStreams.get(streamId);
    if (!stream) {
      return;
    }

    this.tailStreams.delete(streamId);

    try {
      await this.router.sendToRemoteAgent(stream.profileId, "tail_stop", {
        stream_id: streamId,
      });
    } catch {
      // Best-effort stop — the stream may already be gone on the remote
    }

    this.emit("stream-end", { streamId });
  }

  /**
   * Check whether a tail stream is currently active.
   */
  isTailActive(streamId: string): boolean {
    return this.tailStreams.has(streamId);
  }

  // ── Command Execution ───────────────────────────────────────────

  /**
   * Execute a build/run command on the remote server. Calls the agent's
   * "exec_command" RPC and streams stdout/stderr back via "stream-data"
   * events keyed by the returned stream ID.
   *
   * Output is delivered within 1 second of being produced on the remote.
   * Emits "stream-end" with exit code when the command completes.
   */
  executeCommand(profileId: string, command: string, cwd: string): string {
    this.assertConnected(profileId);

    const streamId = randomUUID();

    this.execStreams.set(streamId, { profileId });

    // Build RPC params
    const params: Record<string, unknown> = {
      command,
      cwd,
      stream_id: streamId,
    };

    // Fire the RPC call asynchronously — output arrives via notifications
    this.router
      .sendToRemoteAgent(profileId, "exec_command", params)
      .then((result) => {
        const stream = this.execStreams.get(streamId);
        if (!stream) {
          // Already killed or ended
          return;
        }
        if (stream.killTimer) clearTimeout(stream.killTimer);
        this.execStreams.delete(streamId);

        const response = result as { exit_code?: number; error?: string };
        this.emit("stream-end", {
          streamId,
          exitCode: response.exit_code ?? 0,
          error: response.error,
        });
      })
      .catch((err) => {
        const stream = this.execStreams.get(streamId);
        if (!stream) {
          // Already killed or ended
          return;
        }
        if (stream.killTimer) clearTimeout(stream.killTimer);
        this.execStreams.delete(streamId);

        const message = err instanceof Error ? err.message : String(err);
        this.emit("stream-end", {
          streamId,
          error: message,
        });
      });

    return streamId;
  }

  /**
   * Kill a running remote command. Sends SIGTERM to the process first.
   * If the process does not exit within 5 seconds, sends SIGKILL.
   */
  killCommand(streamId: string): void {
    const stream = this.execStreams.get(streamId);
    if (!stream) return;

    // Send SIGTERM
    this.router
      .sendToRemoteAgent(stream.profileId, "exec_kill", {
        stream_id: streamId,
        signal: "SIGTERM",
      })
      .catch(() => {
        // Best-effort — the process may already be gone
      });

    // Set up the 5-second grace period before escalating to SIGKILL
    stream.killTimer = setTimeout(() => {
      // If the stream is still active after 5s, escalate to SIGKILL
      const current = this.execStreams.get(streamId);
      if (!current) return;

      this.router
        .sendToRemoteAgent(current.profileId, "exec_kill", {
          stream_id: streamId,
          signal: "SIGKILL",
        })
        .catch(() => {
          // Best-effort — force-end the stream locally if SIGKILL RPC fails
          const stillActive = this.execStreams.get(streamId);
          if (stillActive) {
            this.execStreams.delete(streamId);
            this.emit("stream-end", {
              streamId,
              error: "Process killed (SIGKILL)",
            });
          }
        });
    }, KILL_GRACE_MS);
  }

  /**
   * Check whether an exec command stream is currently active.
   */
  isExecActive(streamId: string): boolean {
    return this.execStreams.has(streamId);
  }

  // ── Private Methods ─────────────────────────────────────────────

  /**
   * Index a remote directory tree via agent RPC. Handles up to 50,000
   * entries with a 5-second timeout.
   */
  private async indexDirectoryTree(
    profileId: string,
    remotePath: string,
  ): Promise<{ entries: RemoteDirEntry[]; totalEntries: number; watchMode: WorkspaceState["watchMode"] }> {
    const result = await Promise.race([
      this.router.sendToRemoteAgent(profileId, "workspace_index", {
        path: remotePath,
        max_entries: MAX_TREE_ENTRIES,
      }),
      this.createTimeout(INDEX_TIMEOUT_MS),
    ]);

    const response = result as {
      entries?: Array<{
        path: string;
        kind: string;
        size: number | null;
        pointer_hash?: string | null;
      }>;
      total_entries?: number;
      truncated?: boolean;
      watch_mode?: string;
    };

    const entries: RemoteDirEntry[] = (response.entries ?? []).map((entry) => ({
      path: entry.path,
      kind: entry.kind as RemoteDirEntry["kind"],
      size: entry.size,
      pointer_hash: entry.pointer_hash ?? null,
    }));

    const totalEntries = response.total_entries ?? entries.length;
    const watchMode = this.parseWatchMode(response.watch_mode);

    return { entries, totalEntries, watchMode };
  }

  /**
   * Set up a listener for agent file change notifications to
   * invalidate the cached directory tree, and for search result
   * notifications to forward streaming results.
   */
  private setupNotificationListener(): void {
    this.notificationHandler = (notification) => {
      if (notification.method === "fs_event") {
        this.handleFsEvent(notification.profileId, notification.params);
      } else if (notification.method === "search_result") {
        this.handleSearchResult(notification.params);
      } else if (notification.method === "tail_output") {
        this.handleTailOutput(notification.params);
      } else if (notification.method === "exec_output") {
        this.handleExecOutput(notification.params);
      } else if (notification.method === "exec_done") {
        this.handleExecDone(notification.params);
      }
    };
    this.router.on("notification", this.notificationHandler);
  }

  /**
   * Handle file system events from the remote agent.
   * Invalidates affected cache entries.
   */
  private handleFsEvent(profileId: string, params: unknown): void {
    const state = this.workspaces.get(profileId);
    if (!state) return;

    const event = params as {
      watch_id?: string;
      events?: Array<{ path: string; kind: string; timestamp: number }>;
    };

    if (!event.events || event.events.length === 0) return;

    const changedPaths = event.events.map((e) => e.path);
    this.invalidateCache(profileId, changedPaths);

    // Emit file change events for the UI
    for (const fsEvent of event.events) {
      this.emit("remote-file-changed", {
        profileId,
        path: fsEvent.path,
        event: fsEvent.kind,
      });
    }
  }

  /**
   * Handle search result notifications from the remote agent.
   * Forwards results as "stream-data" events and enforces the
   * 10,000 match cap.
   */
  private handleSearchResult(params: unknown): void {
    const result = params as {
      stream_id?: string;
      data?: string;
      match_count?: number;
    };

    if (!result.stream_id) return;

    const stream = this.activeStreams.get(result.stream_id);
    if (!stream) return;

    // Track match count and enforce cap
    const incomingMatches = result.match_count ?? 1;
    stream.matchCount += incomingMatches;

    this.emit("stream-data", {
      streamId: result.stream_id,
      data: result.data ?? "",
    });

    // Cap at 10,000 matches
    if (stream.matchCount >= SEARCH_MAX_RESULTS) {
      clearTimeout(stream.timer);
      this.activeStreams.delete(result.stream_id);
      this.emit("stream-end", {
        streamId: result.stream_id,
        error: `Search capped at ${SEARCH_MAX_RESULTS} matches`,
      });
    }
  }

  /**
   * Handle tail output notifications from the remote agent.
   * Forwards new lines as "stream-data" events for active tail streams.
   * Delivers new lines within 1 second of being written on the remote.
   */
  private handleTailOutput(params: unknown): void {
    const result = params as {
      stream_id?: string;
      data?: string;
    };

    if (!result.stream_id) return;

    const stream = this.tailStreams.get(result.stream_id);
    if (!stream) return;

    this.emit("stream-data", {
      streamId: result.stream_id,
      data: result.data ?? "",
    });
  }

  /**
   * Handle exec output notifications from the remote agent.
   * Forwards stdout/stderr as "stream-data" events for active exec streams.
   */
  private handleExecOutput(params: unknown): void {
    const result = params as {
      stream_id?: string;
      fd?: "stdout" | "stderr";
      data?: string;
    };

    if (!result.stream_id) return;

    const stream = this.execStreams.get(result.stream_id);
    if (!stream) return;

    this.emit("stream-data", {
      streamId: result.stream_id,
      data: result.data ?? "",
    });
  }

  /**
   * Handle exec done notifications from the remote agent.
   * Emits "stream-end" with exit code when the command completes.
   */
  private handleExecDone(params: unknown): void {
    const result = params as {
      stream_id?: string;
      exit_code?: number;
    };

    if (!result.stream_id) return;

    const stream = this.execStreams.get(result.stream_id);
    if (!stream) return;

    if (stream.killTimer) clearTimeout(stream.killTimer);
    this.execStreams.delete(result.stream_id);

    this.emit("stream-end", {
      streamId: result.stream_id,
      exitCode: result.exit_code ?? 0,
    });
  }

  private isCacheStale(cache: CachedTree): boolean {
    return Date.now() - cache.indexedAt > CACHE_STALE_MS;
  }

  private createTimeout(ms: number): Promise<never> {
    return new Promise((_, reject) => {
      setTimeout(() => reject(new Error(`Directory indexing timed out after ${ms}ms`)), ms);
    });
  }

  private parseWatchMode(mode: string | undefined): WorkspaceState["watchMode"] {
    switch (mode) {
      case "inotify":
        return "inotify";
      case "fsevents":
        return "fsevents";
      case "polling":
        return "polling";
      default:
        return "none";
    }
  }

  private getUnsavedEditsDir(profileId: string): string {
    return path.join(this.unsavedEditsBasePath, profileId);
  }

  /**
   * Convert a remote file path to a safe local filename for persistence.
   * Replaces path separators and special characters with underscores.
   */
  private toSafeFileName(filePath: string): string {
    return filePath
      .replace(/^\//, "")
      .replace(/[/\\:*?"<>|]/g, "_")
      .slice(0, 200); // Limit filename length
  }

  private getParentPath(filePath: string): string | null {
    const lastSlash = filePath.lastIndexOf("/");
    if (lastSlash <= 0) return null;
    return filePath.slice(0, lastSlash);
  }

  private assertConnected(profileId: string): void {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }
  }
}
