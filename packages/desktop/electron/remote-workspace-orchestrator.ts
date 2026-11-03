/**
 * RemoteWorkspaceOrchestrator — orchestrates multi-repo discovery and
 * status polling for remote workspace sources over SSH.
 *
 * Distinct from `RemoteWorkspaceManager` in `ssh-remote-workspace.ts`
 * which handles single-project workspace indexing (directory trees,
 * file editing, search, exec). This class manages workspace-level
 * discovery and batch polling across multiple repos on a remote host.
 */

import { EventEmitter } from "node:events";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

export interface DiscoveryOptions {
  maxDepth?: number;
  excludePatterns?: string[];
}

export interface DiscoveredRepo {
  absolute_path: string;
  relative_path: string;
  name: string;
}

export interface RemoteRepoStatus {
  relative_path: string;
  branch: string | null;
  dirty_count: number;
  ahead: number | null;
  behind: number | null;
  error: string | null;
}

export interface RemoteWorkspace {
  profileId: string;
  rootPath: string;
  repos: DiscoveredRepo[];
}

/**
 * Events emitted:
 * - "discovered" → { profileId, rootPath, repos: DiscoveredRepo[] }
 * - "status-update" → { profileId, statuses: RemoteRepoStatus[], poll_timestamp: number }
 * - "error" → { profileId, message: string }
 */
export class RemoteWorkspaceOrchestrator extends EventEmitter {
  private rpcRouter: RpcRouter;
  private sessionManager: SshSessionManager;
  private pollingIntervals = new Map<string, ReturnType<typeof setInterval>>();
  private openWorkspaces = new Map<string, RemoteWorkspace>();

  constructor(rpcRouter: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.rpcRouter = rpcRouter;
    this.sessionManager = sessionManager;

    // Listen for SSH disconnections to pause polling
    this.sessionManager.on("ssh:status-changed", (payload: { profileId: string; status: string }) => {
      if (payload.status === "disconnected") {
        this.stopPolling(payload.profileId);
        this.emit("error", { profileId: payload.profileId, message: "SSH connection lost" });
      } else if (payload.status === "connected") {
        // Resume polling for any open workspaces on this profile
        const ws = this.openWorkspaces.get(payload.profileId);
        if (ws && ws.repos.length > 0) {
          const repoPaths = ws.repos.map((r) => r.absolute_path);
          this.startPolling(payload.profileId, repoPaths);
        }
      }
    });
  }

  /**
   * Discover git repositories under a remote directory.
   */
  async discoverRepos(
    profileId: string,
    rootPath: string,
    options: DiscoveryOptions = {},
  ): Promise<DiscoveredRepo[]> {
    const params = {
      root_path: rootPath,
      max_depth: options.maxDepth ?? 3,
      exclude_patterns: options.excludePatterns ?? ["node_modules", "vendor", "target", "build"],
    };

    const result = (await this.rpcRouter.sendToRemoteAgent(profileId, "discover_repos", params)) as {
      repos: DiscoveredRepo[];
    };

    const repos = result.repos ?? [];
    this.emit("discovered", { profileId, rootPath, repos });
    return repos;
  }

  /**
   * Start periodic status polling for the given repos on a remote host.
   * Polls every 30 seconds via `batch_git_status` RPC.
   */
  startPolling(profileId: string, repoPaths: string[]): void {
    // Clear any existing interval for this profile
    this.stopPolling(profileId);

    if (repoPaths.length === 0) return;

    const poll = async () => {
      try {
        const result = (await this.rpcRouter.sendToRemoteAgent(profileId, "batch_git_status", {
          repo_paths: repoPaths,
        })) as { statuses: RemoteRepoStatus[]; poll_timestamp: number };

        this.emit("status-update", {
          profileId,
          statuses: result.statuses ?? [],
          poll_timestamp: result.poll_timestamp,
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.emit("error", { profileId, message });
      }
    };

    // Poll immediately, then every 30s
    void poll();
    const interval = setInterval(poll, 30_000);
    this.pollingIntervals.set(profileId, interval);
  }

  /**
   * Stop status polling for a given SSH profile.
   */
  stopPolling(profileId: string): void {
    const interval = this.pollingIntervals.get(profileId);
    if (interval) {
      clearInterval(interval);
      this.pollingIntervals.delete(profileId);
    }
  }

  /**
   * Open a remote workspace: discover repos and start polling.
   */
  async openWorkspace(
    profileId: string,
    rootPath: string,
    options: DiscoveryOptions = {},
  ): Promise<RemoteWorkspace> {
    const repos = await this.discoverRepos(profileId, rootPath, options);

    const workspace: RemoteWorkspace = { profileId, rootPath, repos };
    this.openWorkspaces.set(profileId, workspace);

    // Start polling discovered repos
    const repoPaths = repos.map((r) => r.absolute_path);
    this.startPolling(profileId, repoPaths);

    return workspace;
  }

  /**
   * Close a remote workspace: stop polling and clean up state.
   */
  closeWorkspace(profileId: string): void {
    this.stopPolling(profileId);
    this.openWorkspaces.delete(profileId);
  }

  /**
   * Get the currently open workspace for a profile, if any.
   */
  getWorkspace(profileId: string): RemoteWorkspace | undefined {
    return this.openWorkspaces.get(profileId);
  }

  /**
   * Get all currently open workspaces across all SSH profiles.
   */
  getOpenWorkspaces(): RemoteWorkspace[] {
    return [...this.openWorkspaces.values()];
  }

  /**
   * Stop all polling and clean up. Call on app shutdown.
   */
  dispose(): void {
    for (const [profileId] of this.pollingIntervals) {
      this.stopPolling(profileId);
    }
    this.openWorkspaces.clear();
    this.removeAllListeners();
  }
}
