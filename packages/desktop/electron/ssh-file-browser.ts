/**
 * Remote file browsing over SSH.
 *
 * Provides IPC handlers for browsing the remote filesystem via the
 * RPC router. The remote agent's `list` verb returns directory entries
 * in the same format as local repos, so the renderer can reuse the
 * existing FileTree component.
 */

import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";
import type { RemoteCache } from "./ssh-cache";
import type { ProviderRegistry } from "./provider-registry";
import type { RepoProvider } from "./repo-provider";

// ── Types ────────────────────────────────────────────────────────

export interface RemoteDirEntry {
  path: string;
  kind: "file" | "dir" | "pointer" | "lfs" | "submodule";
  size: number | null;
  pointer_hash: string | null;
}

export interface BrowseResult {
  entries: RemoteDirEntry[];
  path: string;
  hasGitDir: boolean;
}

// ── Remote File Browser ──────────────────────────────────────────

export class RemoteFileBrowser {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  private readonly cache: RemoteCache | null;
  private readonly registry: ProviderRegistry | null;
  private readonly remoteProvider: RepoProvider | null;

  constructor(
    router: RpcRouter,
    sessionManager: SshSessionManager,
    cache?: RemoteCache,
    registry?: ProviderRegistry,
    remoteProvider?: RepoProvider,
  ) {
    this.router = router;
    this.sessionManager = sessionManager;
    this.cache = cache ?? null;
    this.registry = registry ?? null;
    this.remoteProvider = remoteProvider ?? null;
  }

  /**
   * Browse a directory on the remote server. Sends the `list` verb
   * to the remote agent for the given path. Returns entries matching
   * the local DirEntry format.
   */
  async browsePath(profileId: string, remotePath: string): Promise<BrowseResult> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    // Use the remote agent's `list` verb to get directory contents
    const result = await this.router.sendToRemoteAgent(profileId, "browse_path", {
      path: remotePath,
    });

    const response = result as {
      entries: RemoteDirEntry[];
      path: string;
      has_git_dir: boolean;
    };

    return {
      entries: response.entries ?? [],
      path: response.path ?? remotePath,
      hasGitDir: response.has_git_dir ?? false,
    };
  }

  /**
   * Get the remote user's home directory. Sends a command to retrieve
   * `$HOME` on the remote server.
   */
  async getHomeDir(profileId: string): Promise<string> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    const result = await this.router.sendToRemoteAgent(profileId, "home_dir", {});
    const response = result as { path: string };
    return response.path ?? "/home";
  }

  /**
   * Open a remote directory as a repository. Calls `open_local` on
   * the remote agent, which returns a RepoHandle with `is_remote: true`.
   *
   * After a successful open, prefetches `repo_summary`, `list_refs`,
   * `git_status_porcelain`, and `hydration_status` in a single batch
   * RPC call so subsequent UI renders hit the cache.
   */
  async openAsRepo(profileId: string, remotePath: string): Promise<unknown> {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(`SSH connection not available (status: ${status})`);
    }

    const result = await this.router.sendToRemoteAgent(profileId, "open_local", {
      path: remotePath,
    });

    // Register the RemoteProvider for this repo_id in the ProviderRegistry
    // BEFORE returning the result to the renderer (prevents race conditions).
    const repoId = `ssh://${profileId}${remotePath}`;
    if (this.registry && this.remoteProvider && !this.registry.has(repoId)) {
      this.registry.register(repoId, this.remoteProvider);
    }

    // Populate forge_provider on the RepoHandle if the remote agent
    // didn't return it (older agent versions or detection failure).
    const handle = result as Record<string, unknown> | null;
    if (handle && !handle.forge_provider) {
      try {
        const { detectForgeFromUrl } = await import("./forge-detect.js");
        const configResult = await this.router.sendToRemoteAgent(
          profileId,
          "git_config_get",
          { repo_id: repoId, keys: ["remote.origin.url"] },
        ) as { values?: Record<string, string | null> };
        const remoteUrl = configResult?.values?.["remote.origin.url"];
        if (remoteUrl) {
          const forgeProvider = detectForgeFromUrl(remoteUrl);
          if (forgeProvider) {
            handle.forge_provider = forgeProvider;
          }
        }
      } catch {
        // Forge detection is best-effort — don't block repo open.
      }
    }

    // Trigger prefetch in the background — non-blocking so the repo
    // opens immediately while cache warms up.
    if (this.cache) {
      this.cache.prefetch(this.router, repoId).catch(() => {
        // Prefetch failures are non-fatal; data will be fetched on demand.
      });
    }

    return result;
  }
}
