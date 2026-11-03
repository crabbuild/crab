/**
 * Remote Git Manager — performs git operations on remote repositories
 * via the agent RPC interface.
 *
 * Supports detecting git repos, fetching full status (modified, staged,
 * untracked files), and executing git operations (commit, push, pull,
 * checkout, stash) with streaming progress for push/pull.
 *
 * All git operations enforce a 120-second timeout. Commit operations
 * require a non-empty message. Push/pull operations stream progress
 * via "stream-data" and "stream-end" events keyed by a stream ID that
 * the renderer subscribes to.
 */

import { EventEmitter } from "node:events";
import { randomUUID } from "node:crypto";
import type { RpcRouter } from "./ssh-router";
import type { SshSessionManager } from "./ssh-manager";

// ── Constants ────────────────────────────────────────────────────

/** Timeout for all git operations (milliseconds). */
const GIT_OPERATION_TIMEOUT_MS = 120_000;

// ── Types ────────────────────────────────────────────────────────

export interface GitDetectResult {
  isGitRepo: boolean;
  root?: string;
  branch?: string;
  remoteUrl?: string;
}

export interface GitFileStatus {
  path: string;
  status: "modified" | "added" | "deleted" | "renamed" | "copied" | "untracked";
  staged: boolean;
}

export interface GitStatusResult {
  branch: string;
  files: GitFileStatus[];
  ahead: number;
  behind: number;
}

export type GitOperationType = "commit" | "push" | "pull" | "checkout" | "stash";

export interface GitOperation {
  type: GitOperationType;
  /** Required for commit operations. */
  message?: string;
  /** Branch name for checkout operations. */
  branch?: string;
  /** Stash action: "save" | "pop" | "drop" | "list". */
  stashAction?: "save" | "pop" | "drop" | "list";
  /** Repository path on the remote server. */
  repoPath: string;
}

export interface RemoteGitManagerEvents {
  "stream-data": (payload: { streamId: string; data: string }) => void;
  "stream-end": (payload: { streamId: string; exitCode?: number; error?: string }) => void;
}

// ── RemoteGitManager ─────────────────────────────────────────────

export class RemoteGitManager extends EventEmitter {
  private readonly router: RpcRouter;
  private readonly sessionManager: SshSessionManager;
  /** Active stream IDs mapped to their abort controllers for timeout cancellation. */
  private activeStreams = new Map<string, { timer: ReturnType<typeof setTimeout>; profileId: string }>();

  constructor(router: RpcRouter, sessionManager: SshSessionManager) {
    super();
    this.router = router;
    this.sessionManager = sessionManager;

    // Listen for agent notifications to forward streaming progress
    this.router.on("notification", this.handleNotification.bind(this));
  }

  /**
   * Detect whether a path on the remote server is inside a git repository.
   *
   * Calls the agent's "git_detect" RPC method. Returns within 3 seconds
   * under normal conditions (subject to the 120s timeout).
   */
  async detectGitRepo(profileId: string, path: string): Promise<GitDetectResult> {
    this.assertConnected(profileId);

    const result = await this.callWithTimeout(profileId, "git_detect", { path });

    const response = result as {
      is_git_repo?: boolean;
      root?: string;
      branch?: string;
      remote_url?: string;
    };

    return {
      isGitRepo: response.is_git_repo ?? false,
      root: response.root,
      branch: response.branch,
      remoteUrl: response.remote_url,
    };
  }

  /**
   * Get the full git status for a remote repository.
   *
   * Returns the current branch, file statuses (modified, staged, untracked),
   * and ahead/behind counts relative to the tracking branch.
   */
  async getStatus(profileId: string, repoPath: string): Promise<GitStatusResult> {
    this.assertConnected(profileId);

    const result = await this.callWithTimeout(profileId, "git_status_full", {
      repo_path: repoPath,
    });

    const response = result as {
      branch?: string;
      files?: Array<{ path: string; status: string; staged: boolean }>;
      ahead?: number;
      behind?: number;
    };

    const files: GitFileStatus[] = (response.files ?? []).map((f) => ({
      path: f.path,
      status: f.status as GitFileStatus["status"],
      staged: f.staged,
    }));

    return {
      branch: response.branch ?? "unknown",
      files,
      ahead: response.ahead ?? 0,
      behind: response.behind ?? 0,
    };
  }

  /**
   * Execute a git operation on a remote repository.
   *
   * For push/pull operations, returns a stream ID that the renderer can
   * subscribe to for progress updates via "stream-data" events. For other
   * operations, the stream ID is still returned but progress events may
   * not be emitted (the "stream-end" event signals completion).
   *
   * Commit operations require a non-empty message — throws if missing.
   * All operations are subject to a 120-second timeout.
   */
  async executeOperation(profileId: string, operation: GitOperation): Promise<string> {
    this.assertConnected(profileId);

    // Validate commit message requirement
    if (operation.type === "commit") {
      if (!operation.message || operation.message.trim().length === 0) {
        throw new Error("Commit message is required and must not be empty");
      }
    }

    const streamId = randomUUID();

    // Build the RPC params
    const params: Record<string, unknown> = {
      repo_path: operation.repoPath,
      operation: operation.type,
      stream_id: streamId,
    };

    if (operation.message) {
      params.message = operation.message;
    }
    if (operation.branch) {
      params.branch = operation.branch;
    }
    if (operation.stashAction) {
      params.stash_action = operation.stashAction;
    }

    // Set up the 120-second timeout
    const timer = setTimeout(() => {
      this.activeStreams.delete(streamId);
      this.emit("stream-end", {
        streamId,
        error: `Git operation "${operation.type}" timed out after 120 seconds`,
      });
    }, GIT_OPERATION_TIMEOUT_MS);

    this.activeStreams.set(streamId, { timer, profileId });

    // Fire the RPC call asynchronously — don't await for streaming operations
    this.router
      .sendToRemoteAgent(profileId, "git_operation", params)
      .then((result) => {
        const stream = this.activeStreams.get(streamId);
        if (!stream) {
          // Already timed out or ended
          return;
        }
        clearTimeout(stream.timer);
        this.activeStreams.delete(streamId);

        const response = result as { exit_code?: number; error?: string };
        this.emit("stream-end", {
          streamId,
          exitCode: response.exit_code ?? 0,
          error: response.error,
        });
      })
      .catch((err) => {
        const stream = this.activeStreams.get(streamId);
        if (!stream) {
          // Already timed out
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
   * Cancel an active git operation stream (e.g., on user request or shutdown).
   */
  cancelStream(streamId: string): void {
    const stream = this.activeStreams.get(streamId);
    if (stream) {
      clearTimeout(stream.timer);
      this.activeStreams.delete(streamId);
      this.emit("stream-end", {
        streamId,
        error: "Operation cancelled",
      });
    }
  }

  /**
   * Stop all active streams. Useful during graceful shutdown.
   */
  cancelAllStreams(): void {
    for (const [streamId, stream] of this.activeStreams) {
      clearTimeout(stream.timer);
      this.emit("stream-end", {
        streamId,
        error: "Operation cancelled: shutdown",
      });
    }
    this.activeStreams.clear();
  }

  /**
   * Check whether a stream is currently active.
   */
  isStreamActive(streamId: string): boolean {
    return this.activeStreams.has(streamId);
  }

  // ── Private ────────────────────────────────────────────────────

  /**
   * Assert that the SSH connection is active for the given profile.
   * Throws a descriptive error if not connected.
   */
  private assertConnected(profileId: string): void {
    const status = this.sessionManager.getStatus(profileId);
    if (status !== "connected") {
      throw new Error(
        `Cannot perform git operation: SSH connection status is "${status}" for profile "${profileId}"`,
      );
    }
  }

  /**
   * Call an RPC method with the 120-second git operation timeout.
   * Used for non-streaming operations (detect, status).
   */
  private async callWithTimeout(
    profileId: string,
    method: string,
    params: unknown,
  ): Promise<unknown> {
    const timeoutPromise = new Promise<never>((_, reject) => {
      setTimeout(() => {
        reject(new Error(`Git operation "${method}" timed out after 120 seconds`));
      }, GIT_OPERATION_TIMEOUT_MS);
    });

    return Promise.race([
      this.router.sendToRemoteAgent(profileId, method, params),
      timeoutPromise,
    ]);
  }

  /**
   * Handle notifications from the RPC router. Forwards git progress
   * notifications as "stream-data" events for active streams.
   *
   * The agent sends notifications with method "git_progress" containing
   * { stream_id, data } for push/pull progress updates.
   */
  private handleNotification(notification: {
    profileId: string;
    method: string;
    params: unknown;
  }): void {
    if (notification.method !== "git_progress") {
      return;
    }

    const params = notification.params as {
      stream_id?: string;
      data?: string;
    };

    if (!params.stream_id || !this.activeStreams.has(params.stream_id)) {
      return;
    }

    this.emit("stream-data", {
      streamId: params.stream_id,
      data: params.data ?? "",
    });
  }
}
