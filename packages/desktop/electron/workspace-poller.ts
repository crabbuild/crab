import { EventEmitter } from "node:events";
import { parseRepoId } from "./ssh-types";
import { type RpcRouter } from "./ssh-router";
import type { ProviderRegistry } from "./provider-registry";
import type { SshSessionManager } from "./ssh-manager";

export interface RepoStatus {
  relativePath: string;
  branch: string | null;
  dirtyCount: number;
  ahead: number | null;
  behind: number | null;
  error: string | null;
}

export interface PollerOptions {
  intervalMs: number;
  batchSize: number;
}

const DEFAULT_OPTIONS: PollerOptions = {
  intervalMs: 30000,
  batchSize: 10,
};

/**
 * Parses the output of `git status --porcelain -b`.
 *
 * First line format: `## branch...tracking [ahead N, behind M]`
 * or `## branch` (no tracking info).
 * Remaining lines: one per dirty/staged file.
 */
export function parseGitStatus(output: string): {
  branch: string | null;
  dirtyCount: number;
  ahead: number | null;
  behind: number | null;
} {
  const lines = output.split("\n").filter((l) => l.length > 0);

  let branch: string | null = null;
  let ahead: number | null = null;
  let behind: number | null = null;
  let dirtyCount = 0;

  if (lines.length === 0) {
    return { branch, dirtyCount, ahead, behind };
  }

  const headerLine = lines[0];

  if (headerLine.startsWith("## ")) {
    const header = headerLine.slice(3);

    // Parse branch name and tracking info
    // Formats:
    //   "main...origin/main [ahead 2, behind 1]"
    //   "main...origin/main [ahead 2]"
    //   "main...origin/main [behind 1]"
    //   "main...origin/main"
    //   "main"
    //   "HEAD (no branch)" — detached HEAD
    //   "No commits yet on main"

    if (header.startsWith("No commits yet on ")) {
      branch = header.slice("No commits yet on ".length);
    } else if (header === "HEAD (no branch)") {
      branch = null;
    } else {
      // Split on "..." to separate local branch from tracking
      const dotsIndex = header.indexOf("...");
      if (dotsIndex !== -1) {
        branch = header.slice(0, dotsIndex);

        // Look for [ahead N, behind M] or [ahead N] or [behind M]
        const bracketMatch = header.match(
          /\[(?:ahead (\d+))?(?:,\s*)?(?:behind (\d+))?\]$/,
        );
        if (bracketMatch) {
          ahead = bracketMatch[1] ? parseInt(bracketMatch[1], 10) : null;
          behind = bracketMatch[2] ? parseInt(bracketMatch[2], 10) : null;
        }
      } else {
        // No tracking info, just branch name (possibly with spaces trimmed)
        branch = header.trim();
      }
    }

    // Count dirty files (all lines after the header)
    dirtyCount = lines.length - 1;
  }

  return { branch, dirtyCount, ahead, behind };
}

/**
 * Background poller that periodically fetches git status for all
 * workspace repos via the ProviderRegistry and emits status updates.
 *
 * For local repos, the provider delegates to the local agent's
 * `git_status_porcelain` implementation (equivalent to the previous
 * `child_process.execFile("git", ...)` approach).
 *
 * For remote repos, the provider routes through the RpcRouter to the
 * remote agent. Remote calls are batched per SSH connection to
 * minimize round-trips. Disconnected remote repos are skipped and
 * marked with `error: "disconnected"`.
 */
export class StatusPoller extends EventEmitter {
  private rootPath: string;
  private repoPaths: string[];
  private options: PollerOptions;
  private timer: ReturnType<typeof setInterval> | null = null;
  private running = false;
  private pollGeneration = 0;
  private registry: ProviderRegistry | null;
  private rpcRouter: RpcRouter | null;
  private sessionManager: SshSessionManager | null;

  constructor(
    rootPath: string,
    repoPaths: string[],
    options?: Partial<PollerOptions>,
    registry?: ProviderRegistry | null,
    rpcRouter?: RpcRouter | null,
    sessionManager?: SshSessionManager | null,
  ) {
    super();
    this.rootPath = rootPath;
    this.repoPaths = [...repoPaths];
    this.options = { ...DEFAULT_OPTIONS, ...options };
    this.registry = registry ?? null;
    this.rpcRouter = rpcRouter ?? null;
    this.sessionManager = sessionManager ?? null;
  }

  /**
   * Starts polling. Triggers an immediate first poll, then enters
   * the interval loop.
   */
  start(): void {
    if (this.running) return;
    this.running = true;
    const generation = ++this.pollGeneration;
    const publish = (statuses: RepoStatus[]) => {
      // A completed poll belongs to the run that started it. Stopping or
      // restarting must not publish stale results into the next workspace.
      if (this.running && generation === this.pollGeneration) this.emit("status", statuses);
    };

    // Immediate first poll
    void this.pollNow().then(publish);

    this.timer = setInterval(() => {
      void this.pollNow().then(publish);
    }, this.options.intervalMs);
  }

  /** Stops polling and discards pending status publications. */
  stop(): void {
    this.running = false;
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Force an immediate poll (e.g. after a git operation). */
  async pollNow(): Promise<RepoStatus[]> {
    if (!this.registry) {
      // No registry available — fall back to legacy subprocess approach
      return this.pollLegacy();
    }

    return this.pollViaProviders();
  }

  /** Add/remove repos dynamically (e.g. after re-discovery). */
  updateRepos(paths: string[]): void {
    this.repoPaths = [...paths];
  }

  // ── Private: Provider-based polling ────────────────────────────

  /**
   * Poll all repos via the ProviderRegistry. Groups remote repos by
   * SSH profile for batched RPC calls.
   */
  private async pollViaProviders(): Promise<RepoStatus[]> {
    const localRepos: string[] = [];
    const remoteByProfile = new Map<string, string[]>();

    // Classify repos into local vs remote (grouped by SSH profile)
    for (const repoPath of this.repoPaths) {
      const parsed = parseRepoId(repoPath);
      if (parsed) {
        const group = remoteByProfile.get(parsed.profileId) ?? [];
        group.push(repoPath);
        remoteByProfile.set(parsed.profileId, group);
      } else {
        localRepos.push(repoPath);
      }
    }

    // Poll local repos in batches via the provider
    const localResults = await this.pollLocalBatch(localRepos);

    // Poll remote repos batched per SSH connection
    const remoteResults = await this.pollRemoteBatched(remoteByProfile);

    return [...localResults, ...remoteResults];
  }

  /**
   * Poll local repos by shelling out to `git status` directly.
   * This avoids the repo_id mismatch between the poller (which uses
   * relative paths) and the agent's open-repo state (which uses
   * generated IDs like "local-1779253959386-2").
   */
  private async pollLocalBatch(repoPaths: string[]): Promise<RepoStatus[]> {
    const { execFile } = await import("node:child_process");
    const pathMod = await import("node:path");
    const { promisify } = await import("node:util");
    const execFileAsync = promisify(execFile);

    const results: RepoStatus[] = [];

    for (let i = 0; i < repoPaths.length; i += this.options.batchSize) {
      const batch = repoPaths.slice(i, i + this.options.batchSize);
      const batchResults = await Promise.all(
        batch.map(async (relativePath): Promise<RepoStatus> => {
          const repoAbsPath = pathMod.resolve(this.rootPath, relativePath);
          try {
            const { stdout } = await execFileAsync(
              "git",
              ["status", "--porcelain", "-b"],
              { cwd: repoAbsPath, timeout: 10000 },
            );
            const parsed = parseGitStatus(stdout);
            return {
              relativePath,
              branch: parsed.branch,
              dirtyCount: parsed.dirtyCount,
              ahead: parsed.ahead,
              behind: parsed.behind,
              error: null,
            };
          } catch (err: unknown) {
            const message = err instanceof Error ? err.message : String(err);
            return {
              relativePath,
              branch: null,
              dirtyCount: 0,
              ahead: null,
              behind: null,
              error: message,
            };
          }
        }),
      );
      results.push(...batchResults);
    }

    return results;
  }

  /**
   * Poll a single repo via direct git subprocess.
   */
  private async pollSingleRepo(repoPath: string): Promise<RepoStatus> {
    return (await this.pollLocalBatch([repoPath]))[0];
  }

  /**
   * Poll remote repos batched per SSH profile. Uses RpcRouter.batchRpc()
   * to minimize round-trips. Skips disconnected profiles entirely.
   */
  private async pollRemoteBatched(
    remoteByProfile: Map<string, string[]>,
  ): Promise<RepoStatus[]> {
    const results: RepoStatus[] = [];

    for (const [profileId, repoIds] of remoteByProfile) {
      // Check connection status — skip disconnected profiles
      const status = this.sessionManager?.getStatus(profileId);
      if (status !== "connected") {
        // Mark all repos for this profile as disconnected
        for (const repoId of repoIds) {
          results.push({
            relativePath: repoId,
            branch: null,
            dirtyCount: 0,
            ahead: null,
            behind: null,
            error: "disconnected",
          });
        }
        continue;
      }

      // Use batchRpc if available and there are multiple repos
      if (this.rpcRouter && repoIds.length > 1) {
        const batchResults = await this.pollRemoteBatch(profileId, repoIds);
        results.push(...batchResults);
      } else {
        // Single repo or no router — poll individually
        for (const repoId of repoIds) {
          const result = await this.pollSingleRepo(repoId);
          results.push(result);
        }
      }
    }

    return results;
  }

  /**
   * Send a batch of git_status_porcelain calls to a single SSH profile
   * using RpcRouter.batchRpc() for efficiency.
   */
  private async pollRemoteBatch(
    _profileId: string,
    repoIds: string[],
  ): Promise<RepoStatus[]> {
    // Use the first repo_id as the batch target (batchRpc routes by repo_id)
    const calls = repoIds.map((repoId) => ({
      method: "git_status_porcelain",
      params: { repo_id: repoId },
    }));

    try {
      // batchRpc routes based on the first repoId's profile
      const batchResults = await this.rpcRouter!.batchRpc(repoIds[0], calls);

      return repoIds.map((repoId, idx) => {
        const response = batchResults[idx];
        if (response.error) {
          return {
            relativePath: repoId,
            branch: null,
            dirtyCount: 0,
            ahead: null,
            behind: null,
            error: response.error.message,
          };
        }

        const output = typeof response.result === "string"
          ? response.result
          : (response.result as { output?: string })?.output ?? "";
        const parsed = parseGitStatus(output);

        return {
          relativePath: repoId,
          branch: parsed.branch,
          dirtyCount: parsed.dirtyCount,
          ahead: parsed.ahead,
          behind: parsed.behind,
          error: null,
        };
      });
    } catch (err) {
      // Batch failed entirely — mark all repos with the error
      return repoIds.map((repoId) => ({
        relativePath: repoId,
        branch: null,
        dirtyCount: 0,
        ahead: null,
        behind: null,
        error: err instanceof Error ? err.message : String(err),
      }));
    }
  }

  // ── Private: Legacy subprocess polling (fallback) ──────────────

  /**
   * Legacy polling via child_process.execFile. Used when no
   * ProviderRegistry is available (backward compatibility).
   */
  private async pollLegacy(): Promise<RepoStatus[]> {
    const { execFile } = await import("node:child_process");
    const path = await import("node:path");
    const results: RepoStatus[] = [];

    for (let i = 0; i < this.repoPaths.length; i += this.options.batchSize) {
      const batch = this.repoPaths.slice(i, i + this.options.batchSize);
      const batchResults = await Promise.all(
        batch.map((relativePath) => {
          const repoAbsPath = path.resolve(this.rootPath, relativePath);
          return new Promise<RepoStatus>((resolve) => {
            execFile(
              "git",
              ["status", "--porcelain", "-b"],
              { cwd: repoAbsPath, timeout: 10000 },
              (error, stdout, stderr) => {
                if (error) {
                  resolve({
                    relativePath,
                    branch: null,
                    dirtyCount: 0,
                    ahead: null,
                    behind: null,
                    error: stderr?.trim() || error.message,
                  });
                  return;
                }

                const parsed = parseGitStatus(stdout);
                resolve({
                  relativePath,
                  branch: parsed.branch,
                  dirtyCount: parsed.dirtyCount,
                  ahead: parsed.ahead,
                  behind: parsed.behind,
                  error: null,
                });
              },
            );
          });
        }),
      );
      results.push(...batchResults);
    }

    return results;
  }
}
