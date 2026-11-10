/**
 * Remote Provider — RepoProvider implementation for repositories on
 * remote hosts accessed via SSH.
 *
 * Wraps existing SSH modules (RpcRouter, RemoteFileEditor,
 * RemoteGitManager) behind the unified RepoProvider interface. Does
 * NOT reimplement their logic — delegates to them for all operations.
 *
 * Connection loss handling:
 * - Write operations reject immediately with SSH_RECONNECTING
 * - Read operations return stale cached data when available
 * - Capabilities downgrade to noCapabilities() on disconnect
 *
 * File watching:
 * - Subscribes to the RpcRouter's notification stream for `fs/changed`
 * - Re-emits them as provider events with `{ repo_path: repo_id }`
 * - Starts watching automatically when a remote repo is opened
 * - Stops watching on connection loss, restarts on reconnection
 */

import {
  BaseProvider,
  type FsCreateResult,
  type FsDeleteResult,
  type FsReadResult,
  type FsRenameResult,
  type FsWriteResult,
} from "./repo-provider";
import { noCapabilities, type CapabilitySet } from "./repo-capabilities";
import { parseRepoId, type RpcRouter } from "./ssh-router";
import type { RemoteFileEditor } from "./ssh-file-editor";
import type { RemoteGitManager } from "./ssh-remote-git";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { SshSessionManager, SshConnectionStatus } from "./ssh-manager";

// ── Types ────────────────────────────────────────────────────────

export interface RemoteProviderOptions {
  router: RpcRouter;
  fileEditor: RemoteFileEditor;
  gitManager: RemoteGitManager;
  capabilityNegotiator: CapabilityNegotiator;
  sessionManager: SshSessionManager;
}

/** Internal state for a watched remote repo. */
interface WatchState {
  repoId: string;
  profileId: string;
  debounceTimer: ReturnType<typeof setTimeout> | null;
}

// ── Constants ────────────────────────────────────────────────────

/** Debounce interval for remote file change events (matches local behavior). */
const WATCH_DEBOUNCE_MS = 300;

// ── RemoteProvider ───────────────────────────────────────────────

export class RemoteProvider extends BaseProvider {
  private readonly router: RpcRouter;
  private readonly fileEditor: RemoteFileEditor;
  private readonly capabilityNegotiator: CapabilityNegotiator;
  private readonly sessionManager: SshSessionManager;

  /** Cached read results for stale fallback during reconnection. */
  private readCache = new Map<string, { content: string; timestamp: number }>();

  /** Polling interval handle for agents that don't support native file watching. */
  private pollingInterval: ReturnType<typeof setInterval> | null = null;

  /** Last git status output used to detect changes during polling. */
  private lastPollStatus: string | null = null;

  /** The repo_id currently being polled (if any). */
  private pollingRepoId: string | null = null;

  /** Polling interval in milliseconds (30 seconds). */
  private static readonly POLL_INTERVAL_MS = 30_000;

  /** Repos currently being watched, keyed by repo_id. */
  private watchedRepos = new Map<string, WatchState>();

  /** Bound handler for RpcRouter notifications (stored for cleanup). */
  private readonly onNotification: (notification: { profileId: string; method: string; params: unknown }) => void;

  /** Bound handler for SSH connection status changes (stored for cleanup). */
  private readonly onStatusChanged: (payload: { profileId: string; status: SshConnectionStatus; error?: string }) => void;
  private readonly onAgentUnavailable: (payload: { profileId: string }) => void;

  constructor(options: RemoteProviderOptions) {
    super();
    this.router = options.router;
    this.fileEditor = options.fileEditor;
    this.capabilityNegotiator = options.capabilityNegotiator;
    this.sessionManager = options.sessionManager;

    // Subscribe to the RpcRouter's notification stream for fs/changed events
    this.onNotification = this.handleNotification.bind(this);
    this.router.on("notification", this.onNotification);

    // Subscribe to connection status changes to stop/restart watching
    this.onStatusChanged = this.handleStatusChanged.bind(this);
    this.sessionManager.on("ssh:status-changed", this.onStatusChanged);
    this.onAgentUnavailable = ({ profileId }) => {
      this.stopWatchingProfile(profileId);
      void this.emitCapabilityChanges(profileId);
    };
    this.sessionManager.on("ssh:agent-unavailable", this.onAgentUnavailable);
  }

  // ── Core Dispatch ────────────────────────────────────────────

  async request(method: string, params: Record<string, unknown>): Promise<unknown> {
    const repoId = (params.repo_id as string) ?? "";
    const parsed = parseRepoId(repoId);
    if (!parsed) {
      throw new Error(`RemoteProvider.request() called with non-remote repo_id: ${repoId}`);
    }

    if (method === "close") {
      this.sessionManager.forgetRepository(parsed.profileId, repoId);
      if (this.getConnectionStatus(parsed.profileId) !== "connected") return {};
    }
    return this.router.route(repoId, method, params);
  }

  // ── File Watching ────────────────────────────────────────────

  async watchStart(repoId: string): Promise<void> {
    const parsed = parseRepoId(repoId);
    if (!parsed) return;

    const status = this.getConnectionStatus(parsed.profileId);
    if (status !== "connected") return;

    // Track this repo as watched (even before the RPC succeeds, so
    // notifications arriving immediately are handled).
    this.watchedRepos.set(repoId, {
      repoId,
      profileId: parsed.profileId,
      debounceTimer: null,
    });

    const canWatch = this.capabilityNegotiator.isSupported(parsed.profileId, "watch_start", "watch_stop");

    if (canWatch) {
      // Agent supports native file watching — use watch_start RPC
      try {
        await this.router.route(repoId, "watch_start", { repo_id: repoId });
      } catch {
        // Non-fatal — watching is a UX enhancement. The repo remains
        // tracked so reconnection can retry.
      }
    } else {
      // Older agent without file watching — fall back to polling git status
      this.startPolling(repoId, parsed.profileId);
    }
  }

  async watchStop(repoId: string): Promise<void> {
    const parsed = parseRepoId(repoId);
    if (!parsed) return;

    // Clear debounce timer and remove from tracked repos
    const state = this.watchedRepos.get(repoId);
    if (state?.debounceTimer) {
      clearTimeout(state.debounceTimer);
    }
    this.watchedRepos.delete(repoId);

    // Always stop polling if active (regardless of connection status)
    if (this.pollingRepoId === repoId) {
      this.stopPolling();
    }

    const status = this.getConnectionStatus(parsed.profileId);
    if (status !== "connected") return;

    const canWatch = this.capabilityNegotiator.isSupported(parsed.profileId, "watch_start", "watch_stop");
    if (canWatch) {
      try {
        await this.router.route(repoId, "watch_stop", { repo_id: repoId });
      } catch {
        // Non-fatal — best effort cleanup.
      }
    }
  }

  // ── File System Operations ───────────────────────────────────

  async fsCreate(repoId: string, path: string, kind: "file" | "directory"): Promise<FsCreateResult> {
    const parsed = this.assertRemoteAndConnected(repoId);

    if (kind === "file") {
      await this.fileEditor.createFile(parsed.profileId, path);
    } else {
      // Directories are created via the agent RPC since RemoteFileEditor
      // only handles file operations.
      await this.router.sendToRemoteAgent(parsed.profileId, "create_directory", { path });
    }

    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: repoId });
    return { path, kind };
  }

  async fsRename(repoId: string, oldPath: string, newPath: string): Promise<FsRenameResult> {
    const parsed = this.assertRemoteAndConnected(repoId);

    await this.fileEditor.renameFile(parsed.profileId, oldPath, newPath);

    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: repoId });
    return { oldPath, newPath };
  }

  async fsDelete(repoId: string, path: string, _useTrash?: boolean): Promise<FsDeleteResult> {
    const parsed = this.assertRemoteAndConnected(repoId);

    // Remote servers don't have a trash concept — always hard delete.
    await this.fileEditor.deleteFile(parsed.profileId, path);

    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: repoId });
    return { deleted: path, trash: false };
  }

  async fsWrite(repoId: string, path: string, content: string): Promise<FsWriteResult> {
    const parsed = this.assertRemoteAndConnected(repoId);

    const result = await this.fileEditor.writeFile(parsed.profileId, path, content);

    // Cache the write for stale fallback
    this.readCache.set(`${repoId}:${path}`, { content, timestamp: Date.now() });

    this.emit("fs/changed", { type: "fs/changed" as const, repoPath: repoId });
    return { path, written: result.success ? content.length : 0 };
  }

  async fsRead(repoId: string, path: string): Promise<FsReadResult> {
    const parsed = parseRepoId(repoId);
    if (!parsed) {
      throw new Error(`RemoteProvider.fsRead() called with non-remote repo_id: ${repoId}`);
    }

    const status = this.getConnectionStatus(parsed.profileId);

    // During reconnection, return stale cache if available
    if (status === "reconnecting") {
      const cached = this.readCache.get(`${repoId}:${path}`);
      if (cached) {
        return { exists: true, content: cached.content };
      }
      // No cache available — reject
      const err = new Error("Connection lost: file read unavailable during reconnection");
      (err as Error & { code?: string }).code = "SSH_RECONNECTING";
      throw err;
    }

    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    try {
      const result = await this.fileEditor.readFile(parsed.profileId, path);

      // Cache the read for stale fallback
      this.readCache.set(`${repoId}:${path}`, { content: result.content, timestamp: Date.now() });

      return { exists: true, content: result.content };
    } catch (err) {
      // If the error indicates file not found, return exists: false
      const message = err instanceof Error ? err.message : String(err);
      if (message.includes("ENOENT") || message.includes("not found") || message.includes("No such file")) {
        return { exists: false, content: null };
      }
      throw err;
    }
  }

  // ── Capabilities ─────────────────────────────────────────────

  async capabilities(repoId: string): Promise<CapabilitySet> {
    const parsed = parseRepoId(repoId);
    if (!parsed) {
      return noCapabilities();
    }

    const status = this.getConnectionStatus(parsed.profileId);

    if (status !== "connected" || !this.capabilityNegotiator.getCached(parsed.profileId)) return noCapabilities();

    return {
      canEdit: true,
      canWatch: this.capabilityNegotiator.isSupported(parsed.profileId, "watch_start", "watch_stop"),
      canStageHunks: true,
      canWorktree: true,
      canForge: true,
      canHydrate: false, // Remote repos don't support crab hydrate/dehydrate
      canSearch: this.capabilityNegotiator.isSupported(parsed.profileId, "git_grep"),
      canExec: this.capabilityNegotiator.isSupported(parsed.profileId, "exec_command"),
      canTrash: false, // Remote servers have no trash concept
    };
  }

  // ── Lifecycle ────────────────────────────────────────────────

  dispose(): void {
    // Unsubscribe from RpcRouter notifications
    this.router.off("notification", this.onNotification);

    // Unsubscribe from connection status changes
    this.sessionManager.off("ssh:status-changed", this.onStatusChanged);
    this.sessionManager.off("ssh:agent-unavailable", this.onAgentUnavailable);

    // Clear all watch debounce timers
    for (const state of this.watchedRepos.values()) {
      if (state.debounceTimer) clearTimeout(state.debounceTimer);
    }
    this.watchedRepos.clear();

    this.stopPolling();
    this.readCache.clear();
    this.removeAllListeners();
  }

  // ── Private Helpers ──────────────────────────────────────────

  /**
   * Handle notifications from the RpcRouter. Filters for `fs/changed`
   * notifications and re-emits them as provider events with debouncing.
   */
  private handleNotification(notification: { profileId: string; method: string; params: unknown }): void {
    if (notification.method !== "fs/changed") return;

    // Find all watched repos belonging to this profile and emit
    // debounced fs/changed events for them.
    for (const state of this.watchedRepos.values()) {
      if (state.profileId !== notification.profileId) continue;

      // Debounce: reset the timer on each notification, fire once
      // after the burst settles (300ms, matching local behavior).
      if (state.debounceTimer) clearTimeout(state.debounceTimer);
      state.debounceTimer = setTimeout(() => {
        state.debounceTimer = null;
        this.emit("fs/changed", {
          type: "fs/changed" as const,
          repoPath: state.repoId,
        });
      }, WATCH_DEBOUNCE_MS);
    }
  }

  /**
   * Handle SSH connection status changes. Stops watching on connection
   * loss and restarts watching on reconnection. Re-resolves capabilities
   * for all repos on the affected profile and emits `capabilities-changed`
   * events so the renderer can update UI controls reactively.
   */
  private handleStatusChanged(payload: { profileId: string; status: SshConnectionStatus; error?: string }): void {
    const { profileId, status } = payload;

    if (status !== "connected") this.stopWatchingProfile(profileId);

    if (status === "connected") {
      // Connection restored — restart watching for all repos on this profile.
      for (const state of this.watchedRepos.values()) {
        if (state.profileId !== profileId) continue;
        // Fire-and-forget: restart watching in the background
        void this.restartWatching(state.repoId, profileId);
      }
    }

    // Re-resolve capabilities for all watched repos on this profile and
    // emit capabilities-changed events so the renderer updates UI controls.
    void this.emitCapabilityChanges(profileId);
  }

  private stopWatchingProfile(profileId: string): void {
    // Connection lost — stop emitting events for repos on this profile.
    // Clear debounce timers but keep the repos in the watched set so
    // we can restart watching on reconnection.
    for (const state of this.watchedRepos.values()) {
      if (state.profileId !== profileId) continue;
      if (state.debounceTimer) {
        clearTimeout(state.debounceTimer);
        state.debounceTimer = null;
      }
    }

    // Stop polling if it's for a repo on this profile
    if (this.pollingRepoId) {
      const parsed = parseRepoId(this.pollingRepoId);
      if (parsed && parsed.profileId === profileId) {
        this.stopPolling();
      }
    }
  }

  /**
   * Re-resolve capabilities for all repos on the given profile and emit
   * `capabilities-changed` events for each one. Called when the SSH
   * connection state transitions (connected ↔ disconnected ↔ reconnecting).
   */
  private async emitCapabilityChanges(profileId: string): Promise<void> {
    for (const state of this.watchedRepos.values()) {
      if (state.profileId !== profileId) continue;

      const caps = await this.capabilities(state.repoId);
      this.emit("capabilities-changed", {
        type: "capabilities-changed" as const,
        repoId: state.repoId,
        capabilities: caps,
      });
    }
  }

  /**
   * Restart file watching for a repo after reconnection. Sends the
   * `watch_start` RPC to the remote agent (or restarts polling for
   * older agents).
   */
  private async restartWatching(repoId: string, profileId: string): Promise<void> {
    const canWatch = this.capabilityNegotiator.isSupported(profileId, "watch_start", "watch_stop");

    if (canWatch) {
      try {
        await this.router.route(repoId, "watch_start", { repo_id: repoId });
      } catch {
        // Non-fatal — watching is a UX enhancement.
      }
    } else {
      this.startPolling(repoId, profileId);
    }
  }

  /**
   * Start polling git status as a fallback for agents without native
   * file watching. Polls every 30 seconds and emits synthetic
   * `fs/changed` events when the status output changes.
   */
  private startPolling(repoId: string, profileId: string): void {
    // Stop any existing polling first
    this.stopPolling();

    this.pollingRepoId = repoId;
    this.lastPollStatus = null;

    this.pollingInterval = setInterval(() => {
      void this.pollGitStatus(repoId, profileId);
    }, RemoteProvider.POLL_INTERVAL_MS);
  }

  /**
   * Stop the polling interval and reset polling state.
   */
  private stopPolling(): void {
    if (this.pollingInterval !== null) {
      clearInterval(this.pollingInterval);
      this.pollingInterval = null;
    }
    this.pollingRepoId = null;
    this.lastPollStatus = null;
  }

  /**
   * Execute a single poll cycle: call git_status_porcelain and compare
   * the output to the previous result. Emits `fs/changed` if different.
   */
  private async pollGitStatus(repoId: string, profileId: string): Promise<void> {
    const status = this.getConnectionStatus(profileId);
    if (status !== "connected") {
      // Connection lost — stop polling
      this.stopPolling();
      return;
    }

    try {
      const result = await this.router.route(repoId, "git_status_porcelain", { repo_id: repoId });
      const output = typeof result === "string" ? result : JSON.stringify(result);

      if (this.lastPollStatus !== null && output !== this.lastPollStatus) {
        // Status changed — emit synthetic fs/changed event
        this.emit("fs/changed", { type: "fs/changed" as const, repoPath: repoId });
      }

      this.lastPollStatus = output;
    } catch {
      // Polling failure (timeout, connection issue) — silently skip this cycle.
      // If the connection is truly lost, the next cycle's status check will
      // stop polling.
    }
  }

  /**
   * Assert that the repo_id is remote and the connection is active.
   * Throws SSH_RECONNECTING for write operations during reconnection.
   */
  private assertRemoteAndConnected(repoId: string): { profileId: string; remotePath: string } {
    const parsed = parseRepoId(repoId);
    if (!parsed) {
      throw new Error(`RemoteProvider called with non-remote repo_id: ${repoId}`);
    }

    const status = this.getConnectionStatus(parsed.profileId);

    if (status === "reconnecting") {
      const err = new Error("Connection lost: write operations are unavailable during reconnection");
      (err as Error & { code?: string }).code = "SSH_RECONNECTING";
      throw err;
    }

    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    return parsed;
  }

  /**
   * Get the current connection status for a profile.
   */
  private getConnectionStatus(profileId: string): SshConnectionStatus {
    return this.sessionManager.getStatus(profileId);
  }
}
